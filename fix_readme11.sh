#!/bin/bash
cat tla/README.md | grep -n -B 5 -A 20 "AllChildrenRejected"
