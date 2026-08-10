use tiv_core::{
    ids::{EventId, PaymentIntentId},
    trace::CapturedValue,
};

#[test]
fn provider_ids_reject_wrong_prefixes_blank_suffixes_and_unsafe_characters() {
    for invalid in ["", "pi_", "evt_123", "pi_has space", "pi_💳"] {
        assert!(
            PaymentIntentId::new(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
    for invalid in ["", "evt_", "pi_123", "evt_has space", "evt_💳"] {
        assert!(EventId::new(invalid).is_err(), "accepted {invalid:?}");
    }

    assert!(PaymentIntentId::new("pi_tiv_7_1").is_ok());
    assert!(EventId::new("evt_tiv_7_1").is_ok());
}

#[test]
fn provider_ids_revalidate_untrusted_serialized_values() {
    assert!(serde_json::from_str::<PaymentIntentId>(r#""evt_wrong_kind""#).is_err());
    assert!(serde_json::from_str::<EventId>(r#""evt_unsafe value""#).is_err());
}

#[test]
fn captured_values_can_only_be_built_from_validated_provider_ids() {
    assert!(CapturedValue::payment_intent_id("not-a-payment-intent").is_err());
    assert!(CapturedValue::event_id("not-an-event").is_err());
}
