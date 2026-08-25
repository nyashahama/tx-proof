SELECT current_user = 'tiv_invariant'
   AND EXISTS (
       SELECT 1
       FROM payments
       WHERE operation_id = 'op_deadbeef'
   ) AS release_kill;
