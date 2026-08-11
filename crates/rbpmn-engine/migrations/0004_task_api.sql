-- Phase 4: the pull-mode task API. No new tables — pull-mode claims share
-- rbpmn_work_item with the push worker (the design's "both share work_item").
-- This index serves the claim path for BOTH kinds and both FIFO/LIFO
-- directions (btree scans backwards for free); the phase-2 claim index
-- stays for the service-only push worker.

create index rbpmn_work_item_pull on rbpmn_work_item (topic, created_at, item_no)
    where state in ('available', 'locked');
