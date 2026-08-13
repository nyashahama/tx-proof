import json
import os
import urllib.error
import urllib.request

import graphene
import pytest
import stripe


pytestmark = pytest.mark.django_db(
    transaction=True,
    databases=["default", "replica"],
)

pytest_plugins = [
    "saleor.tests.fixtures",
    "saleor.app.tests.fixtures",
    "saleor.plugins.tests.fixtures",
    "saleor.graphql.tests.fixtures",
    "saleor.webhook.tests.fixtures",
    "saleor.tax.tests.fixtures",
    "saleor.channel.tests.fixtures",
    "saleor.page.tests.fixtures",
    "saleor.menu.tests.fixtures",
    "saleor.warehouse.tests.fixtures",
    "saleor.thumbnail.tests.fixtures",
    "saleor.order.tests.fixtures",
    "saleor.product.tests.fixtures",
    "saleor.site.tests.fixtures",
    "saleor.shipping.tests.fixtures",
    "saleor.permission.tests.fixtures",
    "saleor.giftcard.tests.fixtures",
    "saleor.discount.tests.fixtures",
    "saleor.checkout.tests.fixtures",
    "saleor.attribute.tests.fixtures",
    "saleor.account.tests.fixtures",
    "saleor.graphql.account.tests.fixtures",
    "saleor.payment.tests.fixtures",
    "saleor.webhook.tests.circuit_breaker.fixtures",
]


def test_commit_close_retry_creates_two_provider_objects_for_one_saleor_payment(
    settings,
    txproof_payment_stripe_for_checkout,
    channel_USD,
):
    from saleor.payment import PaymentError
    from saleor.payment import gateway as payment_gateway
    from saleor.payment.models import Transaction

    _require_fixture_environment()
    stripe.api_base = os.environ["TIV_STRIPE_API_BASE"].rstrip("/")
    stripe.max_network_retries = 0
    _reset_fixture(["commit_then_close", "normal"])
    manager = _install_stripe_plugin(settings, channel_USD)
    payment = txproof_payment_stripe_for_checkout

    with pytest.raises(PaymentError):
        payment_gateway.process_payment(
            payment,
            token="",
            manager=manager,
            channel_slug=channel_USD.slug,
        )

    payment.refresh_from_db()
    retry_transaction = payment_gateway.process_payment(
        payment,
        token="",
        manager=manager,
        channel_slug=channel_USD.slug,
    )
    payment.refresh_from_db()

    provider_objects = _provider_objects_for_payment(payment.pk)
    provider_ids = {provider["id"] for provider in provider_objects}
    saleor_tokens = set(
        Transaction.objects.filter(payment=payment)
        .exclude(token="")
        .values_list("token", flat=True)
    )

    assert len(provider_objects) == 2
    assert len(provider_ids) == 2
    assert retry_transaction.token in provider_ids
    assert retry_transaction.token in saleor_tokens
    assert len(provider_ids - saleor_tokens) == 1
    assert payment.psp_reference == retry_transaction.token
    assert _provider_uniqueness_violations(payment.pk, provider_objects) == [
        (graphene.Node.to_global_id("Payment", payment.pk), 2, 1)
    ]


@pytest.fixture
def txproof_payment_stripe_for_checkout(
    checkout_with_items,
    address,
    checkout_delivery,
):
    from saleor.checkout import calculations
    from saleor.checkout.fetch import fetch_checkout_info, fetch_checkout_lines
    from saleor.payment.gateways.stripe.plugin import StripeGatewayPlugin
    from saleor.payment.utils import create_payment
    from saleor.plugins.manager import get_plugins_manager

    checkout_with_items.billing_address = address
    checkout_with_items.shipping_address = address
    checkout_with_items.assigned_delivery = checkout_delivery(checkout_with_items)
    checkout_with_items.email = "test@example.com"
    checkout_with_items.save()
    manager = get_plugins_manager(allow_replica=False)
    lines, _ = fetch_checkout_lines(checkout_with_items)
    checkout_info = fetch_checkout_info(checkout_with_items, lines, manager)
    total = calculations.calculate_checkout_total_with_gift_cards(
        manager, checkout_info, lines
    )
    return create_payment(
        gateway=StripeGatewayPlugin.PLUGIN_ID,
        payment_token="ABC",
        total=total.gross.amount,
        currency=checkout_with_items.currency,
        email=checkout_with_items.email,
        customer_ip_address="",
        checkout=checkout_with_items,
    )


def _install_stripe_plugin(settings, channel):
    from saleor.payment.gateways.stripe.plugin import StripeGatewayPlugin
    from saleor.plugins.manager import get_plugins_manager
    from saleor.plugins.models import PluginConfiguration

    settings.PLUGINS = ["saleor.payment.gateways.stripe.plugin.StripeGatewayPlugin"]
    PluginConfiguration.objects.filter(identifier=StripeGatewayPlugin.PLUGIN_ID).delete()
    PluginConfiguration.objects.create(
        identifier=StripeGatewayPlugin.PLUGIN_ID,
        name=StripeGatewayPlugin.PLUGIN_NAME,
        description="",
        active=True,
        channel=channel,
        configuration=[
            {"name": "public_api_key", "value": "pk_test_txproof"},
            {"name": "secret_api_key", "value": "sk_test_txproof"},
            {"name": "automatic_payment_capture", "value": True},
            {"name": "supported_currencies", "value": "USD"},
            {"name": "webhook_endpoint_id", "value": "tiv_test_endpoint"},
            {"name": "webhook_secret_key", "value": "whsec_test_secret"},
            {"name": "include_receipt_email", "value": True},
        ],
    )
    manager = get_plugins_manager(allow_replica=False)
    manager.get_all_plugins()
    return manager


def _provider_objects_for_payment(payment_pk):
    saleor_payment_id = graphene.Node.to_global_id("Payment", payment_pk)
    state = _control("GET", "/v1/control/state")
    return [
        provider
        for provider in state["payment_intents"]
        if provider["metadata"].get("payment_id") == saleor_payment_id
    ]


def _provider_uniqueness_violations(payment_pk, provider_objects):
    from django.db import connection

    values_sql = ", ".join(["(%s, %s)"] * len(provider_objects))
    params = []
    for provider in provider_objects:
        params.extend([provider["id"], provider["metadata"]["payment_id"]])
    params.append(payment_pk)
    with connection.cursor() as cursor:
        cursor.execute(
            f"""
            WITH provider_state(payment_intent_id, saleor_payment_id) AS (
                VALUES {values_sql}
            )
            SELECT
                provider_state.saleor_payment_id,
                COUNT(DISTINCT provider_state.payment_intent_id)::integer
                    AS provider_object_count,
                COUNT(DISTINCT saleor_transaction.token)::integer
                    AS linked_saleor_token_count
            FROM provider_state
            LEFT JOIN payment_transaction AS saleor_transaction
              ON saleor_transaction.payment_id = %s
             AND saleor_transaction.token = provider_state.payment_intent_id
            GROUP BY provider_state.saleor_payment_id
            HAVING COUNT(DISTINCT provider_state.payment_intent_id) > 1
               AND COUNT(DISTINCT saleor_transaction.token)
                   < COUNT(DISTINCT provider_state.payment_intent_id)
            """,
            params,
        )
        return cursor.fetchall()


def _reset_fixture(outcomes):
    state = _control("GET", "/v1/control/state")
    _control(
        "POST",
        "/v1/control/reset",
        {
            "command_sequence": state["command_sequence"] + 1,
            "seed": 42,
            "outcomes": outcomes,
        },
    )


def _control(method, path, payload=None):
    base_url = os.environ["TIV_FIXTURE_CONTROL_URL"].rstrip("/")
    data = json.dumps(payload).encode("utf-8") if payload is not None else None
    request = urllib.request.Request(
        f"{base_url}{path}",
        data=data,
        method=method,
        headers={
            "Content-Type": "application/json",
            "x-tiv-control-token": os.environ["TIV_FIXTURE_CONTROL_TOKEN"],
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.URLError as error:
        pytest.fail(f"TxProof Stripe fixture control request failed: {error}")


def _require_fixture_environment():
    missing = [
        name
        for name in [
            "TIV_FIXTURE_CONTROL_TOKEN",
            "TIV_FIXTURE_CONTROL_URL",
            "TIV_STRIPE_API_BASE",
        ]
        if not os.environ.get(name)
    ]
    if missing:
        pytest.skip(f"requires TxProof Saleor harness environment: {', '.join(missing)}")
