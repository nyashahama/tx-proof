"""Test-only Stripe adapter for the pinned Saleor candidate.

Python imports ``sitecustomize`` during startup when this directory is on
``PYTHONPATH``. The Saleor source remains unmodified; this file only redirects
stripe-python inside the disposable candidate container.
"""

import os

api_base = os.environ.get("TIV_STRIPE_API_BASE")
if api_base:
    import stripe

    stripe.api_base = api_base.rstrip("/")
    stripe.max_network_retries = int(os.environ.get("TIV_STRIPE_MAX_NETWORK_RETRIES", "0"))
