#!/usr/sbin/dtrace -qs
/*
 * Once a second: frames the RDP EGFX worker made and the VNC server
 * sent, their encode time,
 * the guest's fenced flips, and the pace virtio-gpu holds them to: the
 * one the RDP server asks for, or the device's own with no client. The
 * pace is "-" in a second the guest did not flip. Run as
 *   dtrace -qs tools/desk-rate.d -p <rshyve pid>
 *
 * Attaching stops the process briefly to enable the probes: the guest
 * and the client see about half a second of stall when it starts.
 */
vmm$target:::egfx_frame
{
	@frames = count();
	@enc_avg = avg(arg0);
	@enc_max = max(arg0);
	@refused = sum(arg1 == 0);
}

vmm$target:::vnc_frame
{
	@vnc_frames = count();
	@vnc_avg = avg(arg0);
	@vnc_max = max(arg0);
}

/*
 * Enabled, egfx_overlay and vnc_overlay have the RDP and VNC servers
 * draw the same measures over the picture, encode times in ms there,
 * while this script runs.
 */
vmm$target:::egfx_overlay
{
	overlay = 1;
}

vmm$target:::vnc_overlay
{
	overlay = 1;
}

vmm$target:::gpu_flip
{
	@flips = count();
	@held = sum(arg1);
	pace = arg2 / 1000;
}

tick-1s
{
	printa("frames %@d/s  encode avg %@d us max %@d us  refused %@d", @frames, @enc_avg, @enc_max, @refused);
	printa("  vnc frames %@d/s  encode avg %@d us max %@d us", @vnc_frames, @vnc_avg, @vnc_max);
	printa("  guest flips %@d/s  held %@d", @flips, @held);
}

tick-1s
/pace/
{
	printf("  pace %d ms\n", pace);
}

tick-1s
/!pace/
{
	printf("  pace -\n");
}

tick-1s
{
	clear(@frames); clear(@enc_avg); clear(@enc_max); clear(@refused);
	clear(@vnc_frames); clear(@vnc_avg); clear(@vnc_max);
	clear(@flips); clear(@held);
	pace = 0;
}
