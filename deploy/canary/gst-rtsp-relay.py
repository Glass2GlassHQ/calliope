# serves the clean mediamtx's baseline path through gst-rtsp-server, a second rtsp server for the faulted readers
import sys

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstRtsp", "1.0")
gi.require_version("GstRtspServer", "1.0")
from gi.repository import GLib, Gst, GstRtsp, GstRtspServer

SOURCE = "rtsp://127.0.0.1:18554/baseline"
MOUNT = "/faulted"

port = sys.argv[1]
Gst.init(None)
factory = GstRtspServer.RTSPMediaFactory.new()
# config-interval=-1 repeats sps and pps before every idr, so a reader that reconnects can decode
factory.set_launch(
    f"( rtspsrc location={SOURCE} protocols=tcp ! rtph264depay ! h264parse config-interval=-1"
    " ! rtph264pay name=pay0 pt=96 )"
)
factory.set_shared(True)
# the netem filters match the server port, so rtp has to stay interleaved on it
factory.set_protocols(GstRtsp.RTSPLowerTrans.TCP)
server = GstRtspServer.RTSPServer.new()
server.set_service(port)
server.get_mount_points().add_factory(MOUNT, factory)
if server.attach(None) == 0:
    sys.exit(f"gst-rtsp-server cannot listen on port {port}")
GLib.MainLoop().run()
