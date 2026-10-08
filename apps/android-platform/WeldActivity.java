package org.weld.android;

import android.app.NativeActivity;
import android.app.AlertDialog;
import android.content.ClipboardManager;
import android.content.ClipData;
import android.content.Intent;
import android.os.Build;
import android.os.Bundle;
import android.widget.Toast;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.window.OnBackInvokedDispatcher;
import com.google.mlkit.vision.barcode.common.Barcode;
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions;
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning;

/** Android link and clipboard entrypoints; enrollment policy lives in Rust. */
public class WeldActivity extends NativeActivity {
    private String pendingPairingLink;
    private AlertDialog pairingConfirmation;
    private boolean scanning;
    private boolean backRequested;
    private boolean streaming = true;

    protected boolean startsImmersive() { return true; }
    protected String pairingMessage() {
        return "Continue only if you requested this pairing link. Completing pairing replaces your saved desktop connection.";
    }

    public void setStreaming(boolean value) {
        if (streaming != value) { streaming = value; immersive(); }
    }

    public int[] contentInsets() {
        WindowInsets insets = getWindow().getDecorView().getRootWindowInsets();
        if (insets == null) return new int[] {0, 0, 0, 0};
        if (Build.VERSION.SDK_INT >= 30) {
            android.graphics.Insets safe = insets.getInsets(WindowInsets.Type.systemBars()
                | WindowInsets.Type.displayCutout() | WindowInsets.Type.ime());
            return new int[] {safe.left, safe.top, safe.right, safe.bottom};
        }
        return new int[] {insets.getSystemWindowInsetLeft(), insets.getSystemWindowInsetTop(),
            insets.getSystemWindowInsetRight(), insets.getSystemWindowInsetBottom()};
    }

    public void scanPairingCode() {
        if (scanning || pairingConfirmation != null) return;
        scanning = true;
        GmsBarcodeScannerOptions options = new GmsBarcodeScannerOptions.Builder()
            .setBarcodeFormats(Barcode.FORMAT_QR_CODE).enableAutoZoom().build();
        GmsBarcodeScanning.getClient(this, options).startScan()
            .addOnSuccessListener(barcode -> {
                if (!isDestroyed()) confirmPairing(barcode.getRawValue());
            })
            .addOnFailureListener(error -> {
                if (!isDestroyed()) Toast.makeText(this,
                    "Scanner unavailable. Try again after its first download, use your camera app, or paste a pairing link.",
                    Toast.LENGTH_LONG).show();
            })
            .addOnCompleteListener(task -> scanning = false);
    }

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        streaming = startsImmersive();
        WindowManager.LayoutParams attributes = getWindow().getAttributes();
        attributes.layoutInDisplayCutoutMode = Build.VERSION.SDK_INT >= 30
            ? WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
            : WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
        getWindow().setAttributes(attributes);
        if (Build.VERSION.SDK_INT >= 33) {
            getOnBackInvokedDispatcher().registerOnBackInvokedCallback(
                OnBackInvokedDispatcher.PRIORITY_DEFAULT, () -> backRequested = true);
        }
        immersive();
        receive(getIntent());
    }

    @Override public void onWindowFocusChanged(boolean focused) {
        super.onWindowFocusChanged(focused);
        if (focused) immersive();
    }

    private void immersive() {
        if (Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
            WindowInsetsController controller = getWindow().getInsetsController();
            if (controller != null) {
                controller.setSystemBarsBehavior(WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
                if (streaming) controller.hide(WindowInsets.Type.systemBars());
                else controller.show(WindowInsets.Type.systemBars());
            }
        } else {
            getWindow().getDecorView().setSystemUiVisibility(streaming ? (
                View.SYSTEM_UI_FLAG_LAYOUT_STABLE | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_FULLSCREEN
                | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY) : View.SYSTEM_UI_FLAG_LAYOUT_STABLE);
        }
    }

    public boolean takeBackRequest() {
        boolean requested = backRequested;
        backRequested = false;
        return requested;
    }

    @Override public void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        receive(intent);
    }

    private void receive(Intent intent) {
        if (Intent.ACTION_VIEW.equals(intent.getAction()) && intent.getData() != null) {
            confirmPairing(intent.getDataString());
            intent.setData(null);
        }
    }

    private void confirmPairing(String link) {
        if (link == null || link.length() > 4096 || pairingConfirmation != null) return;
        pairingConfirmation = new AlertDialog.Builder(this)
            .setTitle("Pair with a Weld desktop?")
            .setMessage(pairingMessage())
            .setNegativeButton("Cancel", (dialog, which) -> {})
            .setPositiveButton("Continue", (dialog, which) -> pendingPairingLink = link)
            .create();
        pairingConfirmation.setOnDismissListener(dialog -> pairingConfirmation = null);
        pairingConfirmation.show();
    }

    @Override public void onDestroy() {
        if (pairingConfirmation != null) pairingConfirmation.dismiss();
        super.onDestroy();
    }

    public String takePairingLink(boolean paste) {
        if (paste) {
            ClipboardManager clipboard = (ClipboardManager)getSystemService(CLIPBOARD_SERVICE);
            ClipData clip = clipboard.getPrimaryClip();
            if (clip != null && clip.getItemCount() > 0) {
                CharSequence text = clip.getItemAt(0).coerceToText(this);
                if (text != null && text.length() <= 4096) confirmPairing(text.toString());
            }
        }
        String link = pendingPairingLink;
        pendingPairingLink = null;
        return link;
    }

    public String deviceName() { return Build.MODEL; }
    public boolean developmentMode() { return getIntent().getBooleanExtra("weld.development", false); }
}
