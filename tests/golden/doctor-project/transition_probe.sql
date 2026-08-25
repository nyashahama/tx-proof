SELECT current_user = 'tiv_invariant'
   AND EXISTS (SELECT 1 FROM tiv_sql_probe_signal) AS release_kill;
