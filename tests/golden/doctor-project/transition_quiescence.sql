SELECT current_user = 'tiv_invariant'
   AND EXISTS (
       SELECT 1
       FROM tiv_quiescence_signal
   ) AS is_quiescent;
