#!/bin/sh
genhtml .coverage/lcov.info -o .coverage/
open .coverage/index.html
