#!/usr/sbin/dtrace -qs
/*
 * The guest's playback from virtio-snd to the RDP client, once a
 * second: the messages the guest queued and how much audio was already
 * queued ahead of them, underruns, how late the pacer ran, the waves the
 * RDP server made or dropped, and its writes to the client. Each
 * underrun, dropped wave, late pacer step and slow write is also
 * printed as it happens, with the time, so a stutter heard can be
 * matched to the stage it came from. Run as
 *   dtrace -qs tools/audio-rate.d -p <rshyve pid>
 *
 * Attaching stops the process briefly to enable the probes: the guest
 * and the client see about half a second of stall when it starts.
 *
 * An underrun's gap is the silence before a message that came after
 * the playback clock had run out. A stream that went idle and starts
 * again shows the same way, with a gap of the idle time. A summary line
 * shows once its first event has been seen.
 *
 * A slow write with no underrun, late pacer or dropped wave before it
 * points past this process: the client, or the network. In a zone, which
 * has no tcp or mib provider, sample the retransmits alongside with
 *   netstat -s -P tcp | grep tcpRetransSegs
 */

#pragma D option quiet

/* A pacer step this late, or a write that blocked this long, is printed. */
inline int LATE_US = 5000;
inline int SLOW_WRITE_US = 20000;

vmm$target:::snd_tx
{
	@tx = count();
	@tx_bytes = sum(arg0);
	@ahead_min = min(arg1 / 1000);
	@ahead_avg = avg(arg1 / 1000);
}

vmm$target:::snd_tx
/arg2 > 0/
{
	@underruns = count();
	@gap = sum(arg2 / 1000);
	printf("%Y  underrun: %d ms of silence before a %d-byte message\n",
	    walltimestamp, arg2 / 1000, arg0);
}

vmm$target:::snd_play
{
	@play_late = max(arg1);
}

vmm$target:::snd_play
/arg1 > LATE_US/
{
	printf("%Y  pacer late: audio went to the sink %d ms after its start\n",
	    walltimestamp, arg1 / 1000);
}

vmm$target:::snd_done
{
	@done_late = max(arg0);
}

vmm$target:::snd_done
/arg0 > LATE_US/
{
	printf("%Y  pacer late: a message went back %d ms after it played\n",
	    walltimestamp, arg0 / 1000);
}

vmm$target:::rdpsnd_wave
/arg2 == 1/
{
	@waves = count();
}

vmm$target:::rdpsnd_wave
/arg2 == 0/
{
	@dropped = count();
	printf("%Y  wave dropped: the client has %d KiB unread\n",
	    walltimestamp, arg1 / 1024);
}

vmm$target:::rdp_write
{
	@writes = count();
	@write_max = max(arg1);
	@backlog_max = max(arg2 / 1024);
}

vmm$target:::rdp_write
/arg1 > SLOW_WRITE_US/
{
	printf("%Y  slow write: %d bytes took %d ms, %d KiB still unwritten\n",
	    walltimestamp, arg0, arg1 / 1000, arg2 / 1024);
}

tick-1s
{
	printa("snd  %@d msgs/s  %@d B/s  ahead min %@d ms avg %@d ms", @tx,
	    @tx_bytes, @ahead_min, @ahead_avg);
	printa("  underruns %@d  silence %@d ms", @underruns, @gap);
	printa("  pacer late max: play %@d us  done %@d us", @play_late,
	    @done_late);
	printa("  rdp waves %@d/s  dropped %@d", @waves, @dropped);
	printa("  rdp writes %@d/s  longest %@d us  unwritten max %@d KiB\n",
	    @writes, @write_max, @backlog_max);
	/* A cleared min() reads 0 from then on, so it is truncated. */
	trunc(@ahead_min);
	clear(@tx); clear(@tx_bytes); clear(@ahead_avg);
	clear(@underruns); clear(@gap); clear(@play_late); clear(@done_late);
	clear(@waves); clear(@dropped); clear(@writes); clear(@write_max);
	clear(@backlog_max);
}
