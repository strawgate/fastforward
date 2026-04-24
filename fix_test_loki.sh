#!/bin/bash
cat crates/logfwd-output/src/loki.rs | grep -n -B 5 -A 20 "factory_created_sinks_share_timestamp_reservations"
