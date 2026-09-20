#!/bin/sh

set -eu

VM=${VM:-wintest}
VMM=${VMM:-/opt/rust-bhyve/target/release/rshyve}
ISO=${ISO:-/opt/winassets/winsrv2025-eval.iso}
ROM=${ROM:-/usr/share/bhyve/uefi-rom.bin}
VARS=${VARS:-}
VARS_TEMPLATE=${VARS_TEMPLATE:-/opt/fw/BHYVE_UEFI_VARS.pristine.fd}
MEM=${MEM:-4G}
CPUS=${CPUS:-2}

# This runs as root in the global zone and the subcommands have to find
# the same files across separate invocations, so the run directory is a
# fixed name under a root-owned, 0700 directory rather than a predictable
# name in /tmp. illumos has no protected symlinks, so a planted symlink
# at a /tmp name would redirect `mkfile` and `cp` here.
RUNDIR=${RUNDIR:-/var/run/win-boot-test}
mkdir -p "$RUNDIR"
chmod 700 "$RUNDIR"

DISK=${DISK:-$RUNDIR/${VM}.img}
LOGFILE=${LOGFILE:-$RUNDIR/${VM}.log}
FIFO=${FIFO:-$RUNDIR/${VM}.in}
HOLDER=$RUNDIR/${VM}.holder
VMMPID=$RUNDIR/${VM}.vmmpid

usage() {
	printf 'usage: %s {start|send STRING|log [BYTES]|wait PATTERN [SECS]|status|vars|stop}\n' "$0" >&2
	exit 2
}

# Only the VM this script started. These nodes are shared, so a
# `pkill -9 rshyve` would SIGKILL other people's live guests; the VMM pid
# is recorded at start and the kernel instance is destroyed by name.
teardown() {
	if [ -f "$VMMPID" ]; then
		vmm_pid=$(cat "$VMMPID" 2>/dev/null || :)
		if [ -n "$vmm_pid" ]; then
			# Confirm the pid is still this VM's VMM. A pid is
			# reused, and killing whatever inherited it is the
			# failure this check exists for.
			if pgrep -f "$VMM" 2>/dev/null | grep -qx "$vmm_pid"; then
				kill -9 "$vmm_pid" >/dev/null 2>&1 || :
			fi
		fi
		rm -f "$VMMPID" || :
	fi
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
	if [ -n "$VARS" ]; then
		cp "$VARS_TEMPLATE" "$VARS"
		chmod 600 "$VARS"
		BOOTROM_ARG="bootrom,$ROM,$VARS"
	else
		BOOTROM_ARG="bootrom,$ROM"
	fi
	mkfifo "$FIFO"
	# nohup keeps the holder alive after ssh exits; redirecting stderr lets ssh return.
	nohup sleep 100000 > "$FIFO" < /dev/null 2>/dev/null &
	echo $! > "$HOLDER"
	# There is no input path for fbuf/xhci here, so they would only add vmexits.
	nohup "$VMM" -H -c "$CPUS" -m "$MEM" \
		-s 0,hostbridge -s 4,nvme,"$DISK" -s 5,nvme,"$ISO",ro \
		-s 31,lpc -l "$BOOTROM_ARG" -l com1,stdio "$VM" \
		> "$LOGFILE" 2>&1 < "$FIFO" &
	echo $! > "$VMMPID"
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
		# This VM only. Other rshyve processes on the node belong to
		# other people.
		pid=$(cat "$VMMPID" 2>/dev/null || :)
		if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
			printf 'rshyve pid: %s\n' "$pid"
			ps -o pid,pcpu,args -p "$pid" || :
		else
			printf 'rshyve pid: %s\n' 'not running'
		fi
		ls /dev/vmm/ || :
		;;
	vars)
		[ "$#" -eq 1 ] || usage
		digest -a sha256 "$VARS"
		;;
	stop)
		[ "$#" -eq 1 ] || usage
		teardown
		;;
	*)
		usage
		;;
esac
