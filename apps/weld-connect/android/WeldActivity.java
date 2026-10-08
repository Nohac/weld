package org.weld.connect;

/** Native receiver with a persistent multi-host catalogue. */
public final class WeldActivity extends org.weld.android.WeldActivity {
    @Override protected boolean startsImmersive() { return false; }
    @Override protected String pairingMessage() {
        return "Continue only if you requested this pairing link. Compare the verification code on both devices before approving the connection on your computer.";
    }
}
