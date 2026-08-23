SELECT payments.operation_id,
       COUNT(*)::bigint AS local_payment_count,
       COUNT(DISTINCT payments.stripe_payment_intent_id)::bigint AS provider_object_count,
       ARRAY_AGG(payments.id ORDER BY payments.id) AS payment_row_ids,
       ARRAY_AGG(
           payments.stripe_payment_intent_id
           ORDER BY payments.stripe_payment_intent_id
       ) AS provider_payment_intent_ids
FROM payments
JOIN tiv_provider_state AS provider
  ON provider.payment_intent_id = payments.stripe_payment_intent_id
GROUP BY payments.operation_id
HAVING COUNT(DISTINCT payments.stripe_payment_intent_id) > 1
ORDER BY payments.operation_id;
