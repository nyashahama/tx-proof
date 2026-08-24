SELECT entries.provider_event_id,
       entries.operation_id,
       entries.entry_id,
       entries.currency,
       COUNT(postings.posting_id)::bigint AS posting_count,
       COUNT(postings.posting_id)
           FILTER (WHERE postings.entry_side = 'debit')::bigint
           AS debit_posting_count,
       COUNT(postings.posting_id)
           FILTER (WHERE postings.entry_side = 'credit')::bigint
           AS credit_posting_count,
       COALESCE(
           SUM(postings.amount_minor)
               FILTER (WHERE postings.entry_side = 'debit'),
           0
       )::bigint AS debit_total_minor,
       COALESCE(
           SUM(postings.amount_minor)
               FILTER (WHERE postings.entry_side = 'credit'),
           0
       )::bigint AS credit_total_minor,
       (
           COALESCE(
               SUM(postings.amount_minor)
                   FILTER (WHERE postings.entry_side = 'debit'),
               0
           ) -
           COALESCE(
               SUM(postings.amount_minor)
                   FILTER (WHERE postings.entry_side = 'credit'),
               0
           )
       )::bigint AS imbalance_minor
FROM ledger_entries AS entries
LEFT JOIN ledger_postings AS postings USING (entry_id)
GROUP BY entries.provider_event_id,
         entries.operation_id,
         entries.entry_id,
         entries.currency
HAVING COUNT(postings.posting_id) <> 2
    OR COUNT(postings.posting_id)
           FILTER (WHERE postings.entry_side = 'debit') <> 1
    OR COUNT(postings.posting_id)
           FILTER (WHERE postings.entry_side = 'credit') <> 1
    OR COALESCE(
           SUM(postings.amount_minor)
               FILTER (WHERE postings.entry_side = 'debit'),
           0
       ) <> COALESCE(
           SUM(postings.amount_minor)
               FILTER (WHERE postings.entry_side = 'credit'),
           0
       )
ORDER BY entries.entry_id;
