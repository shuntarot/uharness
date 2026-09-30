CROSS ?= riscv64-unknown-elf-

smoke.elf: smoke.S link.ld
	$(CROSS)gcc -march=rv64imac_zicsr -mabi=lp64 -nostdlib -static -T link.ld -o $@ $<

clean:
	rm -f *.elf
