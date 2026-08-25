SELECT provider.operation_id,
       provider.currency,
       COUNT(DISTINCT provider.payment_intent_id)::bigint
           AS provider_succeeded_count,
       COUNT(DISTINCT payments.stripe_payment_intent_id)
           FILTER (
               WHERE payments.status = 'succeeded'
                 AND payments.currency = provider.currency
           )::bigint AS local_succeeded_count,
       SUM(provider.amount_minor)::bigint
           AS provider_succeeded_amount_minor,
       COALESCE(
           SUM(payments.amount_minor)
               FILTER (
                   WHERE payments.status = 'succeeded'
                     AND payments.currency = provider.currency
               ),
           0
       )::bigint AS local_succeeded_amount_minor,
       ARRAY_AGG(
           provider.payment_intent_id
           ORDER BY provider.payment_intent_id
       ) AS provider_payment_intent_ids,
       COALESCE(
           ARRAY_AGG(payments.id ORDER BY payments.id)
               FILTER (WHERE payments.status = 'succeeded'),
           ARRAY[]::bigint[]
       ) AS local_succeeded_payment_row_ids
FROM tiv_provider_state AS provider
LEFT JOIN payments
  ON payments.operation_id = provider.operation_id
 AND payments.stripe_payment_intent_id = provider.payment_intent_id
WHERE provider.status = 'succeeded'
GROUP BY provider.operation_id, provider.currency
HAVING COUNT(DISTINCT provider.payment_intent_id)
           <> COUNT(DISTINCT payments.stripe_payment_intent_id)
                  FILTER (
                      WHERE payments.status = 'succeeded'
                        AND payments.currency = provider.currency
                  )
    OR SUM(provider.amount_minor)
           <> COALESCE(
                  SUM(payments.amount_minor)
                      FILTER (
                          WHERE payments.status = 'succeeded'
                            AND payments.currency = provider.currency
                      ),
                  0
              )
ORDER BY provider.operation_id, provider.currency;
