/// JPEG chroma subsampling used by image optimization.
enum PdfJpegChromaSubsampling {
  /// Preserve full chroma resolution.
  yuv444('444'),

  /// Halve horizontal chroma resolution.
  yuv422('422'),

  /// Halve horizontal and vertical chroma resolution.
  yuv420('420');

  /// Creates a chroma mode with its bridge representation.
  const PdfJpegChromaSubsampling(this.wireName);

  /// Stable bridge representation.
  final String wireName;
}

/// Controls placement-aware PDF image optimization.
final class PdfImageOptimizationOptions {
  /// Creates validated image optimization controls.
  const PdfImageOptimizationOptions({
    required this.quality,
    required this.targetDpi,
    this.downsampleThreshold = 1.5,
    this.chromaSubsampling = PdfJpegChromaSubsampling.yuv422,
    this.minimumSourceBytes = 128,
    this.minimumSavingRatio = 0.01,
    this.passThroughJpeg = false,
  }) : assert(quality >= 1 && quality <= 100),
       assert(targetDpi >= 0),
       assert(downsampleThreshold >= 1),
       assert(minimumSourceBytes >= 0),
       assert(minimumSavingRatio >= 0 && minimumSavingRatio < 1);

  /// JPEG encoder quality from 1 through 100.
  final int quality;

  /// Target resolution for placed images. Zero disables downsampling.
  final double targetDpi;

  /// Resize only above this multiple of [targetDpi].
  final double downsampleThreshold;

  /// JPEG chroma sampling mode.
  final PdfJpegChromaSubsampling chromaSubsampling;

  /// Ignore image streams smaller than this byte count.
  final int minimumSourceBytes;

  /// Required fractional byte saving before replacement.
  final double minimumSavingRatio;

  /// Keeps an existing JPEG byte-for-byte when it does not need resizing.
  final bool passThroughJpeg;

  /// Serializes this request for the shared bridge.
  Map<String, Object> toWireMap() => <String, Object>{
    'quality': quality,
    'targetDpi': targetDpi,
    'downsampleThreshold': downsampleThreshold,
    'chromaSubsampling': chromaSubsampling.wireName,
    'minimumSourceBytes': minimumSourceBytes,
    'minimumSavingRatio': minimumSavingRatio,
    'passThroughJpeg': passThroughJpeg,
  };
}
