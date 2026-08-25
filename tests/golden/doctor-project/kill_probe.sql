SELECT current_user = 'tiv_invariant'
   AND EXISTS (
       SELECT 1
       FROM payments
       WHERE amount_minor = 2500
         AND currency = 'usd'
   ) AS release_kill;
