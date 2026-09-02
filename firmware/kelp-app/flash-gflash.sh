#!/bin/bash
set -e

ELF_FILE=$1
BIN_FILE="${ELF_FILE%.elf}.bin"

# Convert ELF to binary format
rust-objcopy -O binary "$ELF_FILE" "$BIN_FILE"

# Flash using gflash
gflash /dev/ttyACM1 "$BIN_FILE"
