SELECT current_user = 'tiv_invariant'
   AND NOT EXISTS (
       SELECT 1
       FROM pg_stat_activity
       WHERE datname = current_database()
         AND usename = 'tiv_app'
         AND pid <> pg_backend_pid()
   ) AS is_quiescent;
