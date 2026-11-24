# Integration tests

`scripts/integration.sh` builds the `testnode` image (the .deb installed with
`apt install ./wanderlan_*.deb`), loads the required kernel modules into the
host kernel and runs `run.sh`, which starts the topology from `compose.yml`
and executes the cases in `cases/` in order.

Topology:

| Container  | Underlay (transport)  | Trunk network  |
|------------|-----------------------|----------------|
| router     | .254 in every underlay | –             |
| hub1       | 10.100.1.10           | 10.200.1.0/24  |
| hub2       | 10.100.2.10           | 10.200.2.0/24  |
| spoke1     | 10.100.11.10          | 10.200.11.0/24 |
| spoke2     | 10.100.12.10          | 10.200.12.0/24 |
| spoke3     | 10.100.13.10 (MTU 1400) | 10.200.13.0/24 |
| c-<site>   | –                     | client on the trunk of <site> |

The underlay networks stand in for the transport network (usually a VPN)
that wanderlan expects to exist; the router connects them. On every site the
trunk interface is renamed to `trunk0`. Clients add VLAN subinterfaces
(`trunk0.<vid>`) with addresses `10.<vid>.0.<n>`.

Options: `KEEP=1` keeps the containers running, `CASES="10 20"` runs only
some cases.
