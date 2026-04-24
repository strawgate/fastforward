#!/bin/bash
cat crates/logfwd-output/src/loki.rs | grep -n -B 5 -A 20 "fn do_send"
