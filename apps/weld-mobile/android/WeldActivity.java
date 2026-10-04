package org.weld.mobile;

import android.app.NativeActivity;
import android.app.AlertDialog;
import android.content.ClipboardManager;
import android.content.ClipData;
import android.content.Intent;
import android.os.Build;
import android.os.Bundle;
import android.widget.Toast;
import com.google.mlkit.vision.barcode.common.Barcode;
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions;
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning;

/** Android link and clipboard entrypoints; enrollment policy lives in Rust. */
public final class WeldActivity extends NativeActivity {
    private String pendingPairingLink;
    private AlertDialog pairingConfirmation;
    private boolean scanning;

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
        receive(getIntent());
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
            .setMessage("Continue only if you requested this pairing link. Completing pairing replaces your saved desktop connection.")
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
