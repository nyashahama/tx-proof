SELECT provider.operation_id,
       COUNT(payments.id)::bigint AS local_payment_count,
       COUNT(DISTINCT provider.payment_intent_id)::bigint AS provider_object_count,
       COALESCE(
           ARRAY_AGG(payments.id ORDER BY payments.id)
               FILTER (WHERE payments.id IS NOT NULL),
           ARRAY[]::bigint[]
       ) AS payment_row_ids,
       ARRAY_AGG(
           provider.payment_intent_id
           ORDER BY provider.payment_intent_id
       ) AS provider_payment_intent_ids
FROM tiv_provider_state AS provider
LEFT JOIN payments
  ON payments.operation_id = provider.operation_id
 AND payments.stripe_payment_intent_id = provider.payment_intent_id
GROUP BY provider.operation_id
HAVING COUNT(DISTINCT provider.payment_intent_id) > 1
ORDER BY provider.operation_id;
