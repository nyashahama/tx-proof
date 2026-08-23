SELECT payments.operation_id,
       payments.stripe_payment_intent_id,
       payments.status AS local_status,
       provider.status AS provider_status
FROM payments
JOIN tiv_provider_state AS provider
  ON provider.payment_intent_id = payments.stripe_payment_intent_id
WHERE payments.status = 'succeeded'
  AND provider.status <> 'succeeded';
