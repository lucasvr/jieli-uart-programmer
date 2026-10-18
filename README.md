# jieli-uart-programmer

Flashes JieLi SH55 chips (AD15N family, e.g. the AD152A4) through the boot ROM's single-wire UART
on PB9. The binary is called `jieli-uart`.

## What you need

- An ESP32-C6-DevKitC-1 running the `jieli-programmer` firmware, connected to the host through
  its port labelled **USB**.
- A DPS150 bench supply powering the chip at 4.0 V, controlled from the host. `dps150tool` must
  be in `PATH`.
- JieLi's AD15N SDK (`fw-AD15N`), which provides the RAM loader (`sh55loader.uart`) and the
  second-stage bootloader (`uboot.boot`).

The dongle and the supply are found through `/dev/serial/by-id`. Use `--dongle`, `--psu` and
`--sdk` (or `JIELI_DONGLE`, `JIELI_PSU` and `JIELI_SDK`) to override the defaults.

## Wiring

All logic is 3.3 V. The chip's VDDIO is about 3.0 V and PB9 is 5 V tolerant, so no level shifting
is needed.

```
          ESP32-C6-DevKitC-1                              AD152A4 (SOP16)

  GPIO4  (UART RX) ------------------------------+
  GPIO5  (UART TX, open drain) ------------------+
  GPIO2  (line sense) ---------------------------+-------- pin 11  PB9
  GPIO7  (strong driver, optional) --------------+
  GPIO6  (key, ROM header, pull) ---[ 1 kΩ ]-----+

  GPIO10 (VDDIO sense) ------------------+------------------ pin 6   VDDIO
                                         |
                                      [ 10 kΩ ]
                                         |
  GND -----------------------------------+------------------ pin 8   VSS ---- DPS150 -

  DPS150 + (4.0 V) ------------------------------------------ pin 7   VBAT (battery contacts)
```

- The 1 kΩ on GPIO6 lets the key pin override the ROM's weak pull-down without fighting the chip
  when it drives PB9.
- The dongle times the key from the rising edge on GPIO10. The 10 kΩ keeps that pin low while
  the chip is off.
- `jieli-uart` does not use GPIO7. It is only needed for diagnostics.

Check the wiring with the chip powered off, since a running app drives PB9.

## Usage

```
jieli-uart info                       # flash id, size and chip keys
jieli-uart read backup.bin            # back up the whole flash
jieli-uart write app.bin              # pack app.bin and write it to flash
jieli-uart run app.bin                # write, then print the console output
jieli-uart console                    # power-cycle and print the console output
jieli-uart pack app.bin --key 0x1e91 -o image.bin   # build a flash image offline
jieli-uart check image.bin            # verify the CRCs of a flash image
jieli-uart power cycle                # on, off or cycle the supply
```

`app.bin` is a flat binary linked at 0x100100. `write` and `run` wrap it in a flash image, along
with the SDK's `uboot.boot`, and scramble it with the chip's flash key. Use `write --raw --addr
<addr>` to write a file to flash unmodified, for example to restore a backup at address 0.

Each command that talks to the chip power-cycles it with the DPS150. The dongle then sends the
key and pushes the RAM loader, and the flash is accessed through the loader at 100 kbaud. If the
loader does not start, `--attempts` and `--off-secs` control the retries.
