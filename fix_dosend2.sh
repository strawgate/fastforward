#!/bin/bash
cat crates/logfwd-output/src/loki.rs | grep -n -B 5 -A 25 "let (payloads, previous_timestamps) = match self.prepare_and_reserve_payloads"
