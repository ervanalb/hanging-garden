#!/bin/bash

set -euox pipefail

wlink flash "$1" --enable-sdi-print
defmt-print -e "$1" --show-skipped-frames serial --path /dev/ttyACM0
#cat /dev/ttyACM0 | tee out.txt
