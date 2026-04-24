#!/bin/bash
echo "Looking at StreamBuilder scratch..."
grep -A 5 -B 5 "let mut values = vec!\[0i64; num_rows\];" crates/logfwd-arrow/src/streaming_builder/finish.rs
echo "..."
