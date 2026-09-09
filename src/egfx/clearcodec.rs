//! Application-owned ClearCodec input packing; wire encoding belongs to IronRDP.
use anyhow::{ensure, Result};
use ironrdp_graphics::clearcodec::ClearCodecEncoder;
use ironrdp_pdu::geometry::ExclusiveRectangle;
use ironrdp_server::PixelFormat;

pub(super) struct ClearCodecState {
    encoder: ClearCodecEncoder,
    // An encoder cannot be rolled back through the public dependency API.
    // A post-encode transport failure must end this session, never skip sequence numbers.
    uncommitted: bool,
}

pub(super) struct ClearCodecRectangle {
    pub(super) destination: ExclusiveRectangle,
    pub(super) data: Vec<u8>,
}

impl ClearCodecState {
    pub(super) fn encode(
        &mut self,
        input: &ClearCodecInput<'_>,
    ) -> Result<Vec<ClearCodecRectangle>> {
        ensure!(
            !self.uncommitted,
            "ClearCodec frame was not committed; new client required"
        );
        self.uncommitted = true;
        Ok(input
            .rectangles
            .iter()
            .map(|rect| {
                let pixels = input.pixels(rect);
                let data =
                    self.encoder
                        .encode(&pixels, rect.right - rect.left, rect.bottom - rect.top);
                ClearCodecRectangle {
                    destination: rect.clone(),
                    data,
                }
            })
            .collect())
    }

    /// Commit only after every encoded rectangle was enqueued for transport.
    pub(super) fn commit(&mut self) {
        self.uncommitted = false;
    }
}

impl Default for ClearCodecState {
    fn default() -> Self {
        Self {
            encoder: ClearCodecEncoder::new(),
            uncommitted: false,
        }
    }
}

pub(super) struct ClearCodecInput<'a> {
    data: &'a [u8],
    stride: usize,
    pixel_format: PixelFormat,
    pub(super) rectangles: Vec<ExclusiveRectangle>,
}

impl<'a> ClearCodecInput<'a> {
    pub(super) fn new(
        data: &'a [u8],
        width: u16,
        height: u16,
        stride: usize,
        damage: &[(i32, i32, i32, i32)],
        pixel_format: PixelFormat,
    ) -> Result<Self> {
        ensure!(
            matches!(
                pixel_format,
                PixelFormat::BgrA32
                    | PixelFormat::BgrX32
                    | PixelFormat::RgbA32
                    | PixelFormat::RgbX32
            ),
            "unsupported ClearCodec input format: {pixel_format:?}"
        );
        ensure!(width > 0 && height > 0, "empty ClearCodec frame");
        ensure!(
            stride >= usize::from(width) * 4,
            "ClearCodec stride is too short"
        );
        let len = stride
            .checked_mul(usize::from(height))
            .ok_or_else(|| anyhow::anyhow!("ClearCodec buffer length overflow"))?;
        ensure!(data.len() >= len, "ClearCodec buffer is too short");
        // Exact union by horizontal bands. Unlike a bounding box, this never includes gaps.
        let clipped: Vec<_> = damage
            .iter()
            .filter_map(|&(x, y, w, h)| {
                if w <= 0 || h <= 0 {
                    return None;
                }
                let l = i64::from(x).clamp(0, i64::from(width)) as u16;
                let t = i64::from(y).clamp(0, i64::from(height)) as u16;
                let r = (i64::from(x) + i64::from(w)).clamp(0, i64::from(width)) as u16;
                let b = (i64::from(y) + i64::from(h)).clamp(0, i64::from(height)) as u16;
                (l < r && t < b).then_some((l, t, r, b))
            })
            .collect();
        let mut ys: Vec<_> = clipped.iter().flat_map(|&(_, t, _, b)| [t, b]).collect();
        ys.sort_unstable();
        ys.dedup();
        let mut rectangles = Vec::new();
        for band in ys.windows(2) {
            let (top, bottom) = (band[0], band[1]);
            let mut spans: Vec<_> = clipped
                .iter()
                .filter(|&&(_, t, _, b)| t <= top && b >= bottom)
                .map(|&(l, _, r, _)| (l, r))
                .collect();
            spans.sort_unstable();
            let mut merged: Vec<(u16, u16)> = Vec::new();
            for (l, r) in spans {
                if let Some(last) = merged.last_mut().filter(|last| l <= last.1) {
                    last.1 = last.1.max(r);
                } else {
                    merged.push((l, r));
                }
            }
            // 64 is only a scratch-memory bound, not a ClearCodec alignment requirement.
            for (left, right) in merged {
                for y in (top..bottom).step_by(64) {
                    for x in (left..right).step_by(64) {
                        rectangles.push(ExclusiveRectangle {
                            left: x,
                            top: y,
                            right: right.min(x.saturating_add(64)),
                            bottom: bottom.min(y.saturating_add(64)),
                        });
                    }
                }
            }
        }
        Ok(Self {
            data,
            stride,
            pixel_format,
            rectangles,
        })
    }

    fn pixels(&self, rect: &ExclusiveRectangle) -> Vec<u8> {
        let row_bytes = usize::from(rect.right - rect.left) * 4;
        let mut result = Vec::with_capacity(row_bytes * usize::from(rect.bottom - rect.top));
        for y in rect.top..rect.bottom {
            let start = usize::from(y) * self.stride + usize::from(rect.left) * 4;
            result.extend_from_slice(&self.data[start..start + row_bytes]);
        }
        if matches!(self.pixel_format, PixelFormat::RgbA32 | PixelFormat::RgbX32) {
            for pixel in result.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_graphics::clearcodec::ClearCodecDecoder;
    use proptest::prelude::*;

    #[test]
    fn clearcodec_padded_stride_decodes_only_requested_pixels() {
        let data: Vec<_> = (0..8 * 48).map(|i| (i % 251) as u8).collect();
        let input = ClearCodecInput::new(
            &data,
            10,
            8,
            48,
            &[(1, 2, 3, 4)],
            ironrdp_server::PixelFormat::BgrA32,
        )
        .unwrap();
        let mut encoder = ClearCodecEncoder::new();
        let mut decoder = ClearCodecDecoder::new();
        for r in &input.rectangles {
            let pixels = input.pixels(r);
            let encoded = encoder.encode(&pixels, r.right - r.left, r.bottom - r.top);
            let decoded = decoder
                .decode(&encoded, r.right - r.left, r.bottom - r.top)
                .unwrap();
            for (a, b) in pixels
                .as_chunks::<4>()
                .0
                .iter()
                .zip(decoded.as_chunks::<4>().0)
            {
                assert_eq!(&a[..3], &b[..3]);
            }
        }
    }

    #[test]
    fn clearcodec_uncommitted_encode_requires_new_session() {
        let data = vec![31; 8 * 8 * 4];
        let input = ClearCodecInput::new(
            &data,
            8,
            8,
            32,
            &[(0, 0, 8, 8)],
            ironrdp_server::PixelFormat::BgrA32,
        )
        .unwrap();
        let mut state = ClearCodecState::default();
        assert_eq!(state.encode(&input).unwrap()[0].data[1], 0);
        // Any post-encode return before transport commit makes retry unsafe.
        assert!(state.encode(&input).is_err());
        let mut state = ClearCodecState::default();
        assert_eq!(state.encode(&input).unwrap()[0].data[1], 0);
        state.commit();
        assert_eq!(state.encode(&input).unwrap()[0].data[1], 1);
    }

    #[test]
    fn clearcodec_rejects_invalid_buffer_before_encoding() {
        assert!(
            ClearCodecInput::new(&[0; 16], 2, 2, 8, &[(0, 0, 2, 2)], PixelFormat::ARgb32).is_err()
        );
        assert!(
            ClearCodecInput::new(&[], 0, 1, 4, &[], ironrdp_server::PixelFormat::BgrA32).is_err()
        );
        assert!(
            ClearCodecInput::new(&[0; 16], 2, 2, 7, &[], ironrdp_server::PixelFormat::BgrA32)
                .is_err()
        );
        assert!(
            ClearCodecInput::new(&[0; 15], 2, 2, 8, &[], ironrdp_server::PixelFormat::BgrA32)
                .is_err()
        );
        assert!(ClearCodecInput::new(
            &[],
            1,
            2,
            usize::MAX,
            &[],
            ironrdp_server::PixelFormat::BgrA32
        )
        .is_err());
        assert!(
            ClearCodecInput::new(&[0; 16], 2, 2, 8, &[], ironrdp_server::PixelFormat::BgrA32)
                .unwrap()
                .rectangles
                .is_empty()
        );
    }

    proptest! {
        #[test]
        fn generated_clearcodec_rectangles_preserve_exact_damage_union(
            width in 1u16..80, height in 1u16..80, padding in 0usize..16,
            damage in prop::collection::vec((-20i32..100,-20i32..100,-2i32..100,-2i32..100),0..20)
        ) {
            let stride=usize::from(width)*4+padding;
            let data=vec![0;stride*usize::from(height)];
            let input=ClearCodecInput::new(&data,width,height,stride,&damage, ironrdp_server::PixelFormat::BgrA32).unwrap();
            let mut hits=vec![0u8;usize::from(width)*usize::from(height)];
            for r in &input.rectangles {
                prop_assert!(r.right-r.left<=64 && r.bottom-r.top<=64);
                for y in r.top..r.bottom { for x in r.left..r.right { hits[usize::from(y)*usize::from(width)+usize::from(x)]+=1; } }
            }
            for y in 0..height { for x in 0..width {
                let expected=damage.iter().any(|&(l,t,w,h)| w>0 && h>0 && i32::from(x)>=l && i32::from(x)<l+w && i32::from(y)>=t && i32::from(y)<t+h);
                prop_assert_eq!(hits[usize::from(y)*usize::from(width)+usize::from(x)],u8::from(expected));
            } }
        }
    }
}
