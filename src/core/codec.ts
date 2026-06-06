export type VideoCodec = "H264" | "H265";

export function normalizeCodec(raw: unknown): VideoCodec {
  return raw === "H265" ? "H265" : "H264";
}
