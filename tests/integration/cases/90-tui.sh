# shellcheck shell=bash
# The TUI in a real 80x24 terminal (tmux) against the running daemon.

begin "TUI: views in an 80x24 terminal"

x site1 "tmux kill-server 2>/dev/null; tmux new-session -d -s wl -x 80 -y 24 wanderlan"
wait_for 15 "the overview shows the live peers of site1" \
  x site1 "tmux capture-pane -p -t wl | grep -q 'Direct peers (live)' && tmux capture-pane -p -t wl | grep -q 'Network: 5 sites'"
check "the whole screen is drawn in 80x24 (key help in the last line)" \
  x site1 "[ \"\$(tmux capture-pane -p -t wl | wc -l)\" = 24 ] && tmux capture-pane -p -t wl | tail -n 1 | grep -q 'help'"
x site1 "tmux send-keys -t wl 2"
wait_for 10 "key 2 opens the sites view" x site1 "tmux capture-pane -p -t wl | grep -q 'Sites (draft)'"
x site1 "tmux send-keys -t wl 3"
wait_for 10 "key 3 opens the VLAN view with the mappings of site5" \
  x site1 "tmux capture-pane -p -t wl | grep -q 'VLAN 130 at site5 = shared VLAN 30'"
x site1 "tmux send-keys -t wl 5"
wait_for 10 "key 5 opens the live log" x site1 "tmux capture-pane -p -t wl | grep -qi 'log'"
x site1 "tmux send-keys -t wl 7"
wait_for 10 "key 7 opens the settings" x site1 "tmux capture-pane -p -t wl | grep -q 'Settings (Enter: change'"
# Down to "Trunk" (the seventh row), Enter: the interfaces of site1.
x site1 "tmux send-keys -t wl Down Down Down Down Down Down Enter"
uplink=$(x site1 "ip -o route get 10.100.2.10 | sed -n 's/.* dev \([^ ]*\).*/\1/p'")
wait_for 10 "Enter on Trunk lists the interfaces of site1, trunk0 is the main port" \
  x site1 "tmux capture-pane -p -t wl | grep -q 'Trunk ports of this host' && tmux capture-pane -p -t wl | grep -q '\[x\] 1  trunk0 '"
check "trunk1 can be chosen as a further port" \
  x site1 "tmux capture-pane -p -t wl | grep -Eq '\[ \] +trunk1 '"
check "the interface towards site2 ($uplink) cannot: the tunnels run over it" \
  x site1 "tmux capture-pane -p -t wl | grep -Eq '\[ \] +$uplink .*leads to site2'"
x site1 "tmux send-keys -t wl Escape"
wait_for 10 "Esc closes the window (nothing changed, no question)" \
  x site1 "! tmux capture-pane -p -t wl | grep -q 'Trunk ports of this host'"
x site1 "tmux send-keys -t wl q"
wait_for 10 "q quits the TUI" x site1 "! tmux has-session -t wl 2>/dev/null"

begin "TUI: no daemon"

x site1 "tmux new-session -d -s wl2 -x 80 -y 24 'WANDERLAN_SOCKET=/run/wanderlan/none.sock wanderlan'"
wait_for 10 "the TUI explains that the daemon is not running and keeps running" \
  x site1 "tmux capture-pane -p -t wl2 | grep -q 'Daemon not reachable' && tmux capture-pane -p -t wl2 | grep -q 'Retrying in'"
x site1 "tmux kill-server 2>/dev/null; true"
