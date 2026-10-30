#!/bin/bash
OUT=$1
while [ -f /tmp/sampling.on ]; do
  runuser -u postgres -- psql -tA -F, -d mh_drill -c "
    select extract(epoch from now())::bigint,
           coalesce(phase,'-'),
           coalesce(blocks_done,-1), coalesce(blocks_total,-1),
           coalesce(tuples_done,-1), coalesce(tuples_total,-1),
           (select count(*) from pg_locks where not granted),
           (select count(*) from pg_stat_activity where wait_event_type='Lock')
    from pg_stat_progress_create_index" >> "$OUT" 2>/dev/null
  sleep 5
done
