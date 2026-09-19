import android.media.MediaCodecInfo;
import android.media.MediaCodecList;
import android.os.Build;

/** app_process supplies the Binder pool; all decode operations remain in Rust/NDK. */
public final class WeldCodecProbe {
    private static native int run(String fixture, int expectedFrames, int timestampStep, int depth);

    public static void main(String[] args) {
        if (args.length == 2 && args[0].equals("--codec-info")) {
            describeCodec(args[1]);
            return;
        }
        if (args.length != 3 && args.length != 5) {
            System.err.println("Usage: WeldCodecProbe LIBRARY_DIRECTORY FIXTURE EXPECTED_FRAMES [TIMESTAMP_STEP_US PIPELINE_DEPTH]");
            System.exit(2);
        }
        // Do not rely on linker namespace inheritance of LD_LIBRARY_PATH.
        for (String library : new String[]{"avutil", "avcodec", "avformat", "ffmpeg_android_probe"}) {
            System.load(args[0] + "/lib" + library + ".so");
        }
        System.exit(run(args[1], Integer.parseInt(args[2]),
            args.length == 5 ? Integer.parseInt(args[3]) : 16667,
            args.length == 5 ? Integer.parseInt(args[4]) : 2));
    }

    private static void describeCodec(String name) {
        if (Build.VERSION.SDK_INT < 29) {
            System.out.println("Codec flags unavailable below API29");
            return;
        }
        for (MediaCodecInfo codec : new MediaCodecList(MediaCodecList.ALL_CODECS).getCodecInfos()) {
            if (codec.getName().equals(name)) {
                System.out.println(name + " hardware=" + codec.isHardwareAccelerated()
                    + " software=" + codec.isSoftwareOnly() + " vendor=" + codec.isVendor()
                    + " alias=" + codec.isAlias() + " canonical=" + codec.getCanonicalName());
                if (Build.VERSION.SDK_INT >= 30) {
                    for (String type : codec.getSupportedTypes()) {
                        boolean supported = codec.getCapabilitiesForType(type)
                            .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency);
                        System.out.println("mime=" + type + " low_latency_feature=" + supported);
                    }
                }
                return;
            }
        }
        System.err.println("Codec not listed: " + name);
        System.exit(1);
    }
}
