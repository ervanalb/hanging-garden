#!/bin/bash
set -e

ELF_FILE=$1
BIN_FILE="${ELF_FILE%.elf}.bin"

# Convert ELF to binary format
rust-objcopy -O binary "$ELF_FILE" "$BIN_FILE"

# Flash using dfu-util
dfu-util -d 0483:df11 -a 0 -s 0x08000000:leave -D "$BIN_FILE"
