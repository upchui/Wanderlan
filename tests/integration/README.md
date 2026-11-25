# Integration tests

`scripts/integration.sh` builds the `testnode` image (the .deb installed with
`apt install ./wanderlan_*.deb`), loads the required kernel modules into the
host kernel and runs `run.sh`, which starts the topology from `compose.yml`
and executes the cases in `cases/` in order.

Topology:

| Container | Underlay (transport)    | Trunk network                 |
|-----------|-------------------------|-------------------------------|
| router    | .254 in every underlay  | –                             |
| site1     | 10.100.1.10             | 10.200.1.0/24                 |
| site2     | 10.100.2.10             | 10.200.2.0/24                 |
| site3     | 10.100.11.10            | 10.200.11.0/24                |
| site4     | 10.100.12.10            | 10.200.12.0/24                |
| site5     | 10.100.13.10 (MTU 1400) | 10.200.13.0/24                |
| nat       | 10.100.20.10 (its LAN: 192.168.77.254) | – (a NAT router: masquerade with random ports) |
| natc1     | 192.168.77.11, behind nat | 10.200.31.0/24              |
| natc2     | 192.168.77.12, behind nat | 10.200.32.0/24              |
| c-<site>  | –                       | client on the trunk of <site> |
| looper    | –                       | bridges the trunks of site3 and site4 on request (loop guard) |

The underlay networks stand in for the transport network (usually a VPN)
that wanderlan expects to exist; the router connects them. On every site the
trunk interface is renamed to `trunk0`. Clients add VLAN subinterfaces
(`trunk0.<vid>`) with addresses `10.<vid>.0.<n>`.

natc1 and natc2 reach the others through the NAT, nobody reaches them; they
join site1 in case 92 (clients behind NAT, GRE in UDP).

Options: `KEEP=1` keeps the containers running, `CASES="10 20"` runs only
some cases, `BENCH=1` also measures the throughput and CPU through the
tunnels (case 95; the containers share the host's CPUs, so the numbers only
compare the kinds of tunnel).
