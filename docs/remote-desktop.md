# Remote desktop: virtio-gpu, VNC, RDP and sound

rshyve can serve a Linux guest's desktop, with its sound, to an RDP
client such as the Windows App on macOS, and the same picture to a VNC
client on a Unix socket. It is experimental: it works end to end with
the guest and client named below, and the limits at the end are real.
Option details are in [features.md](features.md#virtio-gpu-vnc-and-rdp).

## How it fits together

```
guest driver       rshyve device       server                     client
------------       -------------       ------                     ------
virtio_gpu    -->  virtio-gpu     -+-> VNC (Unix socket)     -->  VNC viewer
                                   +-> RDP (TCP, TLS)        -->  RDP client
virtio_snd    -->  virtio-snd     ---> RDP (sound)
xhci_hcd      <--  xHCI tablet    <--- VNC, RDP (pointer, wheel)
i8042         <--  PS/2 keyboard  <--- VNC, RDP (keys)
```

- **virtio-gpu** (`-s N,virtio-gpu`) is a 2D VirtIO GPU. The guest
  renders into its own memory and tells the device which rectangle
  changed, so the servers read and send only that.
- **virtio-snd** (`-s N,virtio-snd`) is a playback-only VirtIO sound
  card. The device paces the guest in real time and sends each buffer's
  audio on as its clock reaches it, so the sound leaves the host in step
  with the guest's own audio clock, which players time video against.
- **The VNC server** serves the picture on a 0600 Unix socket: changed
  64x64 tiles only, Raw or ZRLE, one client at a time.
- **The RDP server** (crate `vmm_rdp`, built on IronRDP) serves the
  picture, the sound and the input over TCP, with TLS and one fixed
  account. It sends the picture as RemoteFX Progressive over the
  graphics pipeline when the client opens it.
- **Input** from either server reaches the guest through the xHCI
  tablet (pointer and wheel) and the PS/2 keyboard, which every rshyve
  VM has.

## Running it

A certificate and key for TLS. rustls, which IronRDP uses, refuses an
X.509 version 1 certificate, which some `openssl req` builds produce
unless an extension is added; `subjectAltName` does that and names the
host clients will use:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
    -subj '/CN=vm0 rdp' -addext 'subjectAltName=DNS:vm0.example' \
    -keyout rdp-key.pem -out rdp-cert.pem
chmod 600 rdp-key.pem
```

A password file, so the password stays out of argv:

```sh
umask 077; openssl rand -hex 12 > rdp.pw
```

Then the VM. The display keys are all on the `virtio-gpu` spec, and the
five `rdp` keys are required together:

```sh
rshyve -c 2 -m 2G \
  -s 0,hostbridge \
  -s 4,virtio-blk,/path/to/disk.img \
  -s 6,virtio-net-viona,vm0_net0 \
  -s 7,virtio-gpu,unix=/var/run/rshyve/vm0/gpu-vnc.sock,w=1280,h=800,rdp=0.0.0.0:3389,rdp-cert=/path/rdp-cert.pem,rdp-key=/path/rdp-key.pem,rdp-user=console,rdp-password-file=/path/rdp.pw \
  -s 8,virtio-snd \
  -s 30,xhci \
  -s 31,lpc \
  -l com1,stdio \
  --kernel /path/vmlinuz --initrd /path/initramfs \
  --cmdline "console=ttyS0,115200 root=/dev/vda" \
  vm0
```

Connect an RDP client to port 3389 as `console` with the password from
the file. Expect the client to ask whether to trust the self-signed
certificate.

The bhyve UEFI firmware has no virtio-gpu driver, so nothing appears
until the guest kernel's `virtio_gpu` driver loads. The example boots
the kernel directly; for a UEFI guest, add an `fbuf` to see the
firmware and boot loader.

## The guest

What the guest needs:

- Linux with `virtio_gpu`, `virtio_snd`, `xhci_hcd` and `usbhid`.
- A Wayland compositor or X server on the virtio-gpu output. Best is
  one that puts the pointer on the hardware cursor plane: RDP, and a VNC
  client that takes the Cursor pseudo-encoding, then get the pointer's
  image and draw it at their own mouse position with no round trip. Since Linux 6.8 virtio_gpu offers that
  plane only to a compositor that sets
  `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`; sway (wlroots) does. A
  compositor that draws the pointer into the picture works too, with
  the pointer as laggy as the picture.
- An audio server that opens the card, such as PipeWire with
  WirePlumber; with it, the VirtIO card is the default sink.

The guest this was built and tested against: Alpine Linux with the
`linux-virt` 6.18 kernel, PipeWire (`pipewire`, `wireplumber`,
`pipewire-pulse`, `pipewire-alsa`) and Firefox, under two compositors
started by seatd:

- sway with `WLR_RENDERER=pixman`, which uses the hardware cursor, and
  follows the client's window size with the helper below.
- Weston 14 with the pixman or GL renderer, which drew the pointer into
  the picture in both, though it reports cursor planes. Weston does not
  follow a display hotplug either, so a size requested from a client
  reaches the kernel but takes effect only when Weston restarts.

### Following the client's window size

When a client resizes its window, virtio-gpu offers the guest the new
size as its preferred mode and raises a hotplug. The guest kernel sees
it, but the compositor has to switch, and sway does not on its own.
This helper does it for sway: it watches DRM hotplugs and sets each
connected output to its preferred mode. Save it as
`/usr/local/bin/follow-display`, make it executable, and start it from
sway's config with `exec /usr/local/bin/follow-display` (for example in
`/etc/sway/config.d/follow-display`). It needs `udevadm`, from eudev
or systemd.

```sh
#!/bin/sh
# Follow the mode the host prefers. virtio-gpu raises a hotplug
# with a new preferred mode when a remote client resizes its
# window, and sway does not switch on its own.
# --custom: wlroots keeps the modes it read at start, so a new size
# not among them would fall back to the old preferred mode.
apply() {
  for c in /sys/class/drm/card*-*; do
    [ "$(cat $c/status 2>/dev/null)" = connected ] || continue
    mode=$(head -1 $c/modes)
    [ -n "$mode" ] || continue
    swaymsg -- output "${c##*/card[0-9]-}" mode --custom "$mode"
  done
}
apply
udevadm monitor --udev --subsystem-match=drm | while read -r l; do
  case "$l" in *change*) apply ;; esac
done
```

The mode is set with `--custom` because wlroots keeps the modes it read
when the output appeared, and a size not among them would fall back to
the old preferred mode. In testing, a width that was not a multiple of
8 came out rounded down to one (900 became 896), so the picture can be
a few pixels narrower than the client's window; where the rounding
happens is not confirmed. A desktop that follows hotplug on its own
needs no helper.

## Measuring it

rshyve carries DTrace probes for the display path, under the `vmm`
provider like the others:

| Probe | Fires when | Arguments |
|-------|------------|-----------|
| `egfx_frame` | the RDP EGFX worker made a frame | encode time (us), whether the client took it (0 when it had too many frames unacknowledged or 32 MiB of frames and audio unread, the encode failed, or its connection went), the pace asked of the guest (us) |
| `gpu_flip` | virtio-gpu got a fenced flush | descriptor head, whether its answer waits for the pacer, the pace (us) |
| `gpu_flip_answer` | the pacer answered a flip that waited | descriptor head |
| `egfx_overlay` | the RDP worker checks it on every pass of its loop, up to 60 a second; while it is enabled, the worker draws its numbers over the picture | none |

A probe costs nothing until DTrace enables it, so a running VM can be
measured as it is. `tools/desk-rate.d` prints once a second the frames
made, their encode time, the guest's flips and the pace:

```
dtrace -qs tools/desk-rate.d -p $(pgrep -x rshyve)
```

Enabling the probes stops the process briefly, guest included: the
frame in flight when the script starts took about half a second.

The script also enables `egfx_overlay`, so the RDP client shows the
same measures in the top-left corner of the picture while it runs,
encode times in ms where the script prints us: frames made in the
last second, refused ones included, the average and worst encode
time, the
pace and the frames refused. The text is drawn into the frame the RDP
server encodes, not into the guest's display, and goes away when
DTrace detaches. VNC clients do not see it. To show it without the
script:

```
dtrace -n 'vmm$target:::egfx_overlay { overlay = 1; }' -p $(pgrep -x rshyve)
```

The pace: with no RDP client, virtio-gpu lets a Linux guest flip 30
times a second. With one on the graphics pipeline (EGFX), the RDP
server asks for the rate it is managing to send at, slower when frames
take longer to encode or the client refuses them because it has too
many unacknowledged, or because it has not read 32 MiB of frames and audio
already sent, and the device holds the request between 60 and
10 frames a second. The overlay's PACE is the request; desk-rate.d's
pace is what the device applies. Measured on the lab host with 60 frames a
second video: 60 frames a second in a 1520x913 window, at about 1 ms
encode; 47 to 57 at 3440x1440 full screen, at 8 to 14 ms average
encode, with the pace backing off for a second when the client fell
behind.

## What has been checked

With that guest, from the macOS Windows App over a LAN:

- The picture over EGFX as RemoteFX Progressive, including video
  playing in Firefox.
- Keyboard, pointer and wheel.
- The guest's hardware pointer under sway, drawn by the client.
- Resizing the client's window mid-session: under sway with the helper,
  the guest follows each new size, and the server switches to a surface
  of that size, about 10 to 35 ms after the client asks.
- Sound, and audio/video sync by eye and ear on a sync test video.

Over VNC, with a test client on the Unix socket: the picture, input,
tile updates and client-requested resize.

Not checked: a real VNC viewer with ZRLE, a Windows or FreeRDP client,
any client over a slow or distant link, and a Windows guest.

## Limits

- **One RDP connection and one VNC client at a time.** A second VNC
  client waits for the first to leave.
- **No H.264 or other video codec**, so full-screen video costs more
  bandwidth than it would with one.
- **No NLA.** A wrong password shows in some clients as a generic
  protocol error.
- **No clipboard**, no multiple monitors, no audio capture.
- **Audio latency in the client is not compensated for.** The device
  tells the guest its latency is zero; over a slow link, sound may
  trail the picture by the client's buffering.
- **The display work runs on the vCPU** that asks for it: copies into
  the host's copy of the picture happen inline.
- **Migration drops the clients.** virtio-gpu, virtio-snd and the xHCI
  tablet migrate, but the VNC and RDP servers are per host: clients
  are disconnected and reconnect to the destination, which needs its
  own certificate and password file. A pause loses a fraction of a
  second of queued sound.
- **2D only.** No 3D acceleration (virgl or venus).
