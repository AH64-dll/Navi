"""Exercise the real bounded JPEG pipeline with synthetic video; no portal access."""
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
from desktop import Gst, capture_pipeline


class CapturePipelineTests(unittest.TestCase):
    def test_thirty_fps_input_is_bounded_to_two_fps_and_one_buffer(self):
        Gst.init(None)
        pipeline = Gst.parse_launch(capture_pipeline("videotestsrc num-buffers=60 ! video/x-raw,width=1536,height=864,framerate=30/1", 432))
        sink = pipeline.get_by_name("frame")
        frames = []
        def collect(sink):
            sample = sink.emit("pull-sample")
            frames.append({"timestamp": sample.get_buffer().pts, "size": sample.get_buffer().get_size(), "caps": sample.get_caps().to_string()})
            return Gst.FlowReturn.OK
        sink.connect("new-sample", collect)
        try:
            pipeline.set_state(Gst.State.PLAYING)
            message = pipeline.get_bus().timed_pop_filtered(10 * Gst.SECOND, Gst.MessageType.ERROR | Gst.MessageType.EOS)
            self.assertIsNotNone(message)
            self.assertEqual(message.type, Gst.MessageType.EOS, message.parse_error() if message.type == Gst.MessageType.ERROR else "")
            self.assertGreaterEqual(len(frames), 3)
            self.assertLessEqual(len(frames), 5)
            self.assertTrue(all("width=(int)768" in frame["caps"] and "height=(int)432" in frame["caps"] for frame in frames))
            self.assertTrue(all(b["timestamp"] - a["timestamp"] >= Gst.SECOND // 2 for a, b in zip(frames, frames[1:])))
            self.assertEqual(sink.get_property("max-buffers"), 1)
            report = {"source": "synthetic 1536x864 30 fps; no live PipeWire check", "output_frames": len(frames), "output_fps": 2, "queue_limit": 1, "frames": frames}
            (Path(__file__).resolve().parents[1] / "evidence/capture-pipeline.json").write_text(json.dumps(report, indent=2) + "\n")
        finally:
            pipeline.set_state(Gst.State.NULL)


if __name__ == "__main__":
    unittest.main()
