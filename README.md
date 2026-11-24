# wanderlan

wanderlan stretches VLANs across several sites on layer 2, so that the same
VLANs, subnets and DHCP servers are available everywhere as if it were one
network.

wanderlan does **not** build a VPN. The sites must already reach each other
through an existing transport network, usually a VPN set up beforehand.
wanderlan builds the layer-2 part on top of it: one GRETAP tunnel per peer
site, a Linux bridge with VLAN filtering, VLAN mappings, MSS clamping and loop
protection.

> This project is under construction. Installation, configuration and
> troubleshooting documentation follows with the first release.

## Development

Everything builds and runs in Docker; nothing else is needed on the
development machine.

```sh
scripts/check.sh            # cargo fmt --check, clippy -D warnings, tests
scripts/cargo.sh test       # run any cargo command in the build container
```
