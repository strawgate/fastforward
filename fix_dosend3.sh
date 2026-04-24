#!/bin/bash
cat crates/logfwd-output/src/loki.rs | grep -n -B 5 -A 25 "prepare_and_reserve_payloads(&mut stream_map)"
