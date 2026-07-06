// Minimal fragmented-MP4 muxer for live Annex-B H.264 → MSE.
//
// Why this exists: WebCodecs only exists in *secure contexts*, but the
// normal NebulaDisplay deployment is plain-HTTP on a LAN IP — an insecure
// context — which used to force the bandwidth-hungry JPEG fallback. Media
// Source Extensions ARE available on insecure origins, so muxing the host's
// existing Annex-B H.264 into single-frame fMP4 fragments client-side gives
// those browsers real H.264 with no protocol/server changes.
//
// Output structure (ISO/IEC 14496-12):
//   init segment: ftyp + moov(mvhd trak(tkhd mdia(mdhd hdlr minf(vmhd dinf
//                 stbl(stsd(avc1(avcC)) stts stsc stsz stco)))) mvex(trex))
//   per frame:    moof(mfhd traf(tfhd tfdt trun)) + mdat(AVCC samples)
//
// One frame per fragment = the lowest latency MSE can operate at.

const TIMESCALE = 90000;

// ---------------------------------------------------------------------------
// Box writer
// ---------------------------------------------------------------------------

function concat(parts: Uint8Array[]): Uint8Array {
  const total = parts.reduce((a, p) => a + p.length, 0);
  const out = new Uint8Array(total);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function box(type: string, ...payload: Uint8Array[]): Uint8Array {
  const body = concat(payload);
  const out = new Uint8Array(8 + body.length);
  const dv = new DataView(out.buffer);
  dv.setUint32(0, out.length);
  for (let i = 0; i < 4; i++) out[4 + i] = type.charCodeAt(i);
  out.set(body, 8);
  return out;
}

function fullBox(type: string, version: number, flags: number, ...payload: Uint8Array[]): Uint8Array {
  const head = new Uint8Array(4);
  head[0] = version;
  head[1] = (flags >> 16) & 0xff;
  head[2] = (flags >> 8) & 0xff;
  head[3] = flags & 0xff;
  return box(type, head, ...payload);
}

function u8(...values: number[]): Uint8Array {
  return new Uint8Array(values);
}

function u16(v: number): Uint8Array {
  return u8((v >> 8) & 0xff, v & 0xff);
}

function u32(v: number): Uint8Array {
  return u8((v >>> 24) & 0xff, (v >>> 16) & 0xff, (v >>> 8) & 0xff, v & 0xff);
}

function u64(v: number): Uint8Array {
  const hi = Math.floor(v / 4294967296);
  return concat([u32(hi), u32(v >>> 0)]);
}

// ---------------------------------------------------------------------------
// Annex-B parsing
// ---------------------------------------------------------------------------

/** Split an Annex-B elementary stream into NAL units (without start codes). */
export function splitNals(data: Uint8Array): Uint8Array[] {
  const nals: Uint8Array[] = [];
  let i = 0;
  let start = -1;
  while (i + 2 < data.length) {
    if (data[i] === 0 && data[i + 1] === 0 && (data[i + 2] === 1 || (data[i + 2] === 0 && data[i + 3] === 1))) {
      const scLen = data[i + 2] === 1 ? 3 : 4;
      if (start >= 0) nals.push(data.subarray(start, i));
      start = i + scLen;
      i += scLen;
    } else {
      i++;
    }
  }
  if (start >= 0 && start < data.length) nals.push(data.subarray(start));
  return nals;
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

export class Fmp4Muxer {
  private sps: Uint8Array | null = null;
  private pps: Uint8Array | null = null;
  private seq = 1;
  private baseDts = 0;
  private lastTsUs: bigint | null = null;
  private width = 0;
  private height = 0;

  /** MIME/codec string for MediaSource.isTypeSupported / addSourceBuffer. */
  codecString(): string {
    if (this.sps && this.sps.length >= 4) {
      const p = this.sps;
      const hex = (b: number) => b.toString(16).padStart(2, "0");
      return `video/mp4; codecs="avc1.${hex(p[1]!)}${hex(p[2]!)}${hex(p[3]!)}"`;
    }
    return 'video/mp4; codecs="avc1.42E01F"';
  }

  /**
   * Feed one Annex-B access unit. Returns segments to append: the init
   * segment precedes the first keyframe fragment (and again after a
   * parameter-set change). Returns null while waiting for the first
   * keyframe (SPS/PPS unseen).
   */
  push(
    payload: Uint8Array,
    keyframe: boolean,
    tsUs: bigint,
    width: number,
    height: number,
  ): Uint8Array[] | null {
    const nals = splitNals(payload);
    const media: Uint8Array[] = [];
    let paramsChanged = false;
    for (const nal of nals) {
      const type = nal[0]! & 0x1f;
      if (type === 7) {
        if (!this.sps || !bytesEq(this.sps, nal)) {
          this.sps = nal.slice();
          paramsChanged = true;
        }
      } else if (type === 8) {
        if (!this.pps || !bytesEq(this.pps, nal)) {
          this.pps = nal.slice();
          paramsChanged = true;
        }
      } else if (type !== 9 && type !== 6) {
        media.push(nal); // slices (1/5); drop AUD/SEI (harmless either way)
      } else {
        media.push(nal);
      }
    }
    if (!this.sps || !this.pps) return null; // wait for parameter sets
    if (media.length === 0) return null;

    const out: Uint8Array[] = [];
    if (paramsChanged || (keyframe && this.seq === 1)) {
      this.width = width;
      this.height = height;
      out.push(this.initSegment());
    }

    // Frame duration from capture timestamps (fallback 1/30 s).
    let durUs = 33333;
    if (this.lastTsUs !== null) {
      const d = Number(tsUs - this.lastTsUs);
      if (d > 1000 && d < 1_000_000) durUs = d;
    }
    this.lastTsUs = tsUs;
    const duration = Math.round((durUs * TIMESCALE) / 1_000_000);

    // AVCC: 4-byte length-prefixed NALs.
    const sampleParts: Uint8Array[] = [];
    for (const nal of media) {
      sampleParts.push(u32(nal.length), nal);
    }
    const sample = concat(sampleParts);

    out.push(this.fragment(sample, duration, keyframe));
    return out;
  }

  private initSegment(): Uint8Array {
    const sps = this.sps!;
    const pps = this.pps!;
    const avcC = box(
      "avcC",
      u8(1, sps[1]!, sps[2]!, sps[3]!, 0xff, 0xe1),
      u16(sps.length),
      sps,
      u8(1),
      u16(pps.length),
      pps,
    );
    const avc1 = box(
      "avc1",
      u8(0, 0, 0, 0, 0, 0), // reserved
      u16(1), // data_reference_index
      new Uint8Array(16), // pre_defined/reserved
      u16(this.width),
      u16(this.height),
      u32(0x00480000), // 72 dpi horiz
      u32(0x00480000), // 72 dpi vert
      u32(0),
      u16(1), // frame_count
      new Uint8Array(32), // compressor name
      u16(0x0018), // depth
      u8(0xff, 0xff), // pre_defined = -1
      avcC,
    );
    const stsd = fullBox("stsd", 0, 0, u32(1), avc1);
    const stbl = box(
      "stbl",
      stsd,
      fullBox("stts", 0, 0, u32(0)),
      fullBox("stsc", 0, 0, u32(0)),
      fullBox("stsz", 0, 0, u32(0), u32(0)),
      fullBox("stco", 0, 0, u32(0)),
    );
    const dinf = box("dinf", fullBox("dref", 0, 0, u32(1), fullBox("url ", 0, 1)));
    const vmhd = fullBox("vmhd", 0, 1, new Uint8Array(8));
    const minf = box("minf", vmhd, dinf, stbl);
    const hdlr = fullBox(
      "hdlr",
      0,
      0,
      u32(0),
      str4("vide"),
      new Uint8Array(12),
      strz("NebulaVideo"),
    );
    const mdhd = fullBox("mdhd", 0, 0, u32(0), u32(0), u32(TIMESCALE), u32(0), u16(0x55c4), u16(0));
    const mdia = box("mdia", mdhd, hdlr, minf);
    const tkhd = fullBox(
      "tkhd",
      0,
      7, // enabled | in_movie | in_preview
      u32(0), // creation
      u32(0), // modification
      u32(1), // track id
      u32(0), // reserved
      u32(0), // duration
      new Uint8Array(8), // reserved
      u16(0), // layer
      u16(0), // alternate group
      u16(0), // volume
      u16(0), // reserved
      identityMatrix(),
      u32(this.width << 16),
      u32(this.height << 16),
    );
    const trak = box("trak", tkhd, mdia);
    const mvhd = fullBox(
      "mvhd",
      0,
      0,
      u32(0),
      u32(0),
      u32(TIMESCALE),
      u32(0), // duration unknown (live)
      u32(0x00010000), // rate 1.0
      u16(0x0100), // volume
      u16(0),
      new Uint8Array(8),
      identityMatrix(),
      new Uint8Array(24), // pre_defined
      u32(2), // next track id
    );
    const trex = fullBox("trex", 0, 0, u32(1), u32(1), u32(0), u32(0), u32(0x00010000));
    const moov = box("moov", mvhd, trak, box("mvex", trex));
    const ftyp = box("ftyp", str4("isom"), u32(0x200), str4("isom"), str4("iso5"), str4("avc1"), str4("mp41"));
    return concat([ftyp, moov]);
  }

  private fragment(sample: Uint8Array, duration: number, keyframe: boolean): Uint8Array {
    const seq = this.seq++;
    // Sample flags (ISO 14496-12 §8.8.3): sync = 0x02000000 depends-flags,
    // non-sync = 0x01010000 (depends on others + non-sync-sample bit).
    const flags = keyframe ? 0x02000000 : 0x01010000;
    const tfhd = fullBox(
      "tfhd",
      0,
      0x020000 | 0x20, // default-base-is-moof | default-sample-flags present
      u32(1), // track id
      u32(flags),
    );
    const tfdt = fullBox("tfdt", 1, 0, u64(this.baseDts));
    this.baseDts += duration;
    // trun: data-offset + duration + size present.
    const trunLen = 8 + 4 + 4 + 4 + 4 + 4;
    const trun = fullBox(
      "trun",
      0,
      0x000001 | 0x000100 | 0x000200, // data-offset | duration | size
      u32(1), // sample count
      u32(0), // data offset placeholder (patched below)
      u32(duration),
      u32(sample.length),
    );
    const traf = box("traf", tfhd, tfdt, trun);
    const mfhd = fullBox("mfhd", 0, 0, u32(seq));
    const moof = box("moof", mfhd, traf);
    // Patch trun data_offset: from moof start to mdat payload.
    const dataOffset = moof.length + 8;
    const trunOffsetInMoof = moof.length - trunLen + 8 + 4 + 4; // into moof: trun header(8)+flags handled — compute directly instead:
    void trunOffsetInMoof;
    patchTrunDataOffset(moof, dataOffset);
    const mdat = box("mdat", sample);
    return concat([moof, mdat]);
  }
}

function patchTrunDataOffset(moof: Uint8Array, dataOffset: number): void {
  // Find the trun box within moof and set its data_offset field
  // (trun payload: [version/flags u32][sample_count u32][data_offset i32]).
  for (let i = 0; i + 8 <= moof.length; i++) {
    if (
      moof[i + 4] === 0x74 && // t
      moof[i + 5] === 0x72 && // r
      moof[i + 6] === 0x75 && // u
      moof[i + 7] === 0x6e // n
    ) {
      const dv = new DataView(moof.buffer, moof.byteOffset + i);
      dv.setUint32(16, dataOffset);
      return;
    }
  }
}

function bytesEq(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function str4(s: string): Uint8Array {
  return u8(s.charCodeAt(0), s.charCodeAt(1), s.charCodeAt(2), s.charCodeAt(3));
}

function strz(s: string): Uint8Array {
  const out = new Uint8Array(s.length + 1);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

function identityMatrix(): Uint8Array {
  return concat([
    u32(0x00010000),
    u32(0),
    u32(0),
    u32(0),
    u32(0x00010000),
    u32(0),
    u32(0),
    u32(0),
    u32(0x40000000),
  ]);
}
