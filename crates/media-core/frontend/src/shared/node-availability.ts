import type { NodeSummary } from "@/shared/api/types";

type UploadAvailability = Pick<
  NodeSummary,
  "healthy" | "control_connected" | "connected" | "ffmpeg_alive"
>;

export function isNodeUploadReady(node: UploadAvailability) {
  return (
    node.healthy &&
    node.control_connected &&
    node.connected !== false &&
    node.ffmpeg_alive !== false
  );
}
