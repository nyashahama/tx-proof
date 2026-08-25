SELECT success.operation_id,
       success.stripe_payment_intent_id,
       success.provider_event_id AS success_event_id,
       older.provider_event_id AS older_event_id,
       success.history_id AS success_history_id,
       older.history_id AS older_history_id,
       success.provider_created AS success_provider_created,
       older.provider_created AS older_provider_created,
       success.local_status_after AS success_local_status_after,
       older.local_status_after AS older_local_status_after
FROM payment_status_history AS success
JOIN payment_status_history AS older
  ON older.operation_id = success.operation_id
 AND older.stripe_payment_intent_id = success.stripe_payment_intent_id
WHERE success.observed_status = 'succeeded'
  AND success.local_status_after = 'succeeded'
  AND older.observed_status = 'requires_confirmation'
  AND older.history_id > success.history_id
  AND older.provider_created < success.provider_created
  AND older.applied
  AND older.local_status_after = 'pending'
ORDER BY success.operation_id,
         success.stripe_payment_intent_id,
         success.history_id,
         older.history_id;
