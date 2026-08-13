/// MS-RDPBCGR 2.2.5.1 / "Set Error Info PDU Data" errorInfo codes — just enough of the
/// table to self-diagnose a Set Error Info PDU (pduType2=47) instead of silently
/// disconnecting with no explanation.
pub fn describe(code: u32) -> &'static str {
    match code {
        0x00000000 => "ERRINFO_NONE",
        0x00000001 => "ERRINFO_RPC_INITIATED_DISCONNECT",
        0x00000002 => "ERRINFO_RPC_INITIATED_LOGOFF",
        0x00000003 => "ERRINFO_IDLE_TIMEOUT",
        0x00000004 => "ERRINFO_LOGON_TIMEOUT",
        0x00000005 => "ERRINFO_DISCONNECTED_BY_OTHERCONNECTION",
        0x00000006 => "ERRINFO_OUT_OF_MEMORY",
        0x00000007 => "ERRINFO_SERVER_DENIED_CONNECTION",
        0x00000009 => "ERRINFO_SERVER_INSUFFICIENT_PRIVILEGES",
        0x0000000A => "ERRINFO_SERVER_FRESH_CREDENTIALS_REQUIRED",
        0x0000000B => "ERRINFO_RPC_INITIATED_DISCONNECT_BYUSER",
        0x0000000C => "ERRINFO_LOGOFF_BY_USER",
        0x000010C9 => "ERRINFO_UNKNOWNPDUTYPE2 — server received a pduType2 it didn't recognize",
        0x000010CA => "ERRINFO_UNKNOWNPDUTYPE — server received a pduType it didn't recognize",
        0x000010CB => "ERRINFO_DATAPDUSEQUENCE — out-of-sequence Slow-Path Data PDU",
        0x000010CD => {
            "ERRINFO_CONTROLPDUSEQUENCE — out-of-sequence Demand/Confirm Active, Deactivate All, \
             or Enhanced Security Server Redirection PDU"
        }
        0x000010CE => "ERRINFO_INVALIDCONTROLPDUACTION — Control PDU with invalid action field",
        0x000010CF => "ERRINFO_INVALIDINPUTPDUTYPE — invalid messageType/eventCode in an input event",
        0x000010D0 => "ERRINFO_INVALIDINPUTPDUMOUSE — invalid pointerFlags in a mouse event",
        0x000010D1 => "ERRINFO_INVALIDREFRESHRECTPDU — our Refresh Rect PDU was malformed or out of bounds",
        0x000010D2 => "ERRINFO_CREATEUSERDATAFAILED — server failed to build GCC Conference Create Response",
        0x000010D3 => "ERRINFO_CONNECTFAILED — Channel Connection phase failed",
        0x000010D4 => "ERRINFO_CONFIRMACTIVEWRONGSHAREID — our Confirm Active shareID didn't match",
        0x000010D5 => "ERRINFO_CONFIRMACTIVEWRONGORIGINATOR — our Confirm Active originatorID was wrong",
        0x000010E2 => {
            "ERRINFO_SHAREDATATOOSHORT — malformed Control/Font List PDU or truncated share \
             control/data header"
        }
        0x000010E5 => "ERRINFO_CONFIRMACTIVEPDUTOOSHORT — our Confirm Active PDU was truncated/malformed",
        0x000010E7 => "ERRINFO_CAPABILITYSETTOOSMALL — a capability set's header didn't fit",
        0x000010E8 => "ERRINFO_CAPABILITYSETTOOLARGE — a capability set's lengthCapability exceeded the data received",
        0x000010E9 => "ERRINFO_NOCURSORCACHE — Pointer Capability Set cache sizes both zero",
        0x000010EA => "ERRINFO_BADCAPABILITIES — server rejected our Confirm Active capabilities",
        0x000010F0 => "ERRINFO_VCHANNELSTOOMANY — requested more than 31 static virtual channels",
        0x000010F3 => "ERRINFO_REMOTEAPPSNOTENABLED — server requires INFO_RAIL (RemoteApp-only session)",
        0x00001114 => "ERRINFO_SECURITYDATATOOSHORT5 — Client Info PDU (basic fields) truncated",
        0x00001126 => "ERRINFO_SECURITYDATATOOSHORT23 — Client Info PDU Data truncated overall",
        _ => "(unknown/unmapped error code — see MS-RDPBCGR 2.2.5.1 table)",
    }
}
