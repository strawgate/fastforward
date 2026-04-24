#!/bin/bash
cat tla/FanoutSink.tla | grep -n -B 5 -A 20 "PartialSuccessIsOk"
