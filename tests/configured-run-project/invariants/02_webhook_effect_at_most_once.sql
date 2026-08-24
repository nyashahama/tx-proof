SELECT webhook_effects.provider_event_id,
       webhook_effects.operation_id,
       COUNT(*)::bigint AS application_count,
       ARRAY_AGG(webhook_effects.id ORDER BY webhook_effects.id) AS effect_row_ids
FROM webhook_effects
GROUP BY webhook_effects.provider_event_id, webhook_effects.operation_id
HAVING COUNT(*) > 1
ORDER BY webhook_effects.provider_event_id;
