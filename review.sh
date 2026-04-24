#!/bin/bash
cat tla/FanoutSink.tla | grep -n -B 5 -A 10 "FinalizeTerminal"
