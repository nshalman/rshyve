#!/bin/sh

set -eu

VM=${VM:-wintest}
VMM=${VMM:-/opt/vmmnew/target/release/vmm}
DISK=${DISK:-/tmp/wintest.img}
ISO=${ISO:-/opt/winassets/winsrv2025-eval.iso}
ROM=${ROM:-/usr/share/bhyve/uefi-rom.bin}
LOGFILE=${LOGFILE:-/tmp/${VM}.log}
FIFO=${FIFO:-/tmp/${VM}.in}
MEM=${MEM:-4G}
CPUS=${CPUS:-2}
HOLDER=/tmp/${VM}.holder

usage() {
	printf 'usage: %s {start|send STRING|log [BYTES]|wait PATTERN [SECS]|status|stop}\n' "$0" >&2
	exit 2
}

teardown() {
	# Brackets keep pkill from matching the remote ssh shell that invoked it.
	pkill -9 -f 'relea[s]e/vmm' >/dev/null 2>&1 || :
	/usr/sbin/bhyvectl --vm="$VM" --destroy >/dev/null 2>&1 || :
	if [ -f "$HOLDER" ]; then
		holder_pid=$(cat "$HOLDER" 2>/dev/null || :)
		[ -z "$holder_pid" ] || kill "$holder_pid" >/dev/null 2>&1 || :
		rm -f "$HOLDER" || :
	fi
	rm -f "$FIFO" || :
	sleep 2 || :
}

start() {
	teardown
	[ -f "$DISK" ] || mkfile -n 40g "$DISK"
	mkfifo "$FIFO"
	# nohup keeps the holder alive after ssh exits; redirecting stderr lets ssh return.
	nohup sleep 100000 > "$FIFO" < /dev/null 2>/dev/null &
	echo $! > "$HOLDER"
	# There is no input path for fbuf/xhci here, so they would only add vmexits.
	nohup "$VMM" -H -c "$CPUS" -m "$MEM" \
		-s 0,hostbridge -s 4,nvme,"$DISK" -s 5,nvme,"$ISO",ro \
		-s 31,lpc -l bootrom,"$ROM" -l com1,stdio "$VM" \
		> "$LOGFILE" 2>&1 < "$FIFO" &
	printf '%s\n' "$!"
}

wait_for_log() {
	pattern=$1
	seconds=$2
	elapsed=0
	while [ "$elapsed" -lt "$seconds" ]; do
		if grep -q "$pattern" "$LOGFILE" 2>/dev/null; then
			return 0
		fi
		sleep 2
		elapsed=$((elapsed + 2))
	done
	if grep -q "$pattern" "$LOGFILE" 2>/dev/null; then
		return 0
	fi
	return 1
}

[ "$#" -gt 0 ] || usage

case $1 in
	start)
		[ "$#" -eq 1 ] || usage
		start
		;;
	send)
		[ "$#" -eq 2 ] || usage
		printf '%b' "$2" > "$FIFO"
		;;
	log)
		[ "$#" -le 2 ] || usage
		tail -c "${2:-2000}" "$LOGFILE"
		;;
	wait)
		[ "$#" -ge 2 ] && [ "$#" -le 3 ] || usage
		wait_for_log "$2" "${3:-120}"
		;;
	status)
		[ "$#" -eq 1 ] || usage
		pids=$(pgrep -f 'relea[s]e/vmm' 2>/dev/null || :)
		printf 'vmm pid: %s\n' "${pids:-not running}"
		if [ -n "$pids" ]; then
			ps -o pid,pcpu,args -p $pids || :
		fi
		ls /dev/vmm/ || :
		;;
	stop)
		[ "$#" -eq 1 ] || usage
		teardown
		;;
	*)
		usage
		;;
esac
