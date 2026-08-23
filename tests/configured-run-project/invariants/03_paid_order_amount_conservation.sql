SELECT orders.operation_id,
       orders.amount_minor AS expected_amount_minor,
       payments.amount_minor AS payment_amount_minor,
       orders.currency AS expected_currency,
       payments.currency AS payment_currency
FROM orders
JOIN payments USING (operation_id)
WHERE orders.amount_minor <> payments.amount_minor
   OR orders.currency <> payments.currency;
