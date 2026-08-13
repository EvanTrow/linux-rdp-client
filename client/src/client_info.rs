/// Client Info PDU (MS-RDPBCGR 2.2.1.11). Wrapped in a Basic Security Header (flags=
/// SEC_INFO_PKT) since RDS AAD Auth forces Standard RDP Security encryption off — TLS
/// already provides confidentiality. Domain/UserName/Password are empty: authentication
/// already happened at the RDS AAD Auth layer, there's nothing to auto-logon with here.
pub fn build() -> Vec<u8> {
    let mut out = Vec::new();

    // Basic Security Header: flags(2) = SEC_INFO_PKT, flagsHi(2) = 0.
    out.extend_from_slice(&0x0040u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());

    // TS_INFO_PACKET
    out.extend_from_slice(&0u32.to_le_bytes()); // CodePage (ignored; INFO_UNICODE set below)
    let flags: u32 = 0x0001 // INFO_MOUSE
        | 0x0002 // INFO_DISABLECTRLALTDEL
        | 0x0010 // INFO_UNICODE
        | 0x0040; // INFO_LOGONNOTIFY
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // cbDomain
    out.extend_from_slice(&0u16.to_le_bytes()); // cbUserName
    out.extend_from_slice(&0u16.to_le_bytes()); // cbPassword
    out.extend_from_slice(&0u16.to_le_bytes()); // cbAlternateShell
    out.extend_from_slice(&0u16.to_le_bytes()); // cbWorkingDir
    out.extend_from_slice(&[0, 0]); // Domain: just the null terminator (Unicode)
    out.extend_from_slice(&[0, 0]); // UserName
    out.extend_from_slice(&[0, 0]); // Password
    out.extend_from_slice(&[0, 0]); // AlternateShell
    out.extend_from_slice(&[0, 0]); // WorkingDir

    // TS_EXTENDED_INFO_PACKET
    out.extend_from_slice(&2u16.to_le_bytes()); // clientAddressFamily: AF_INET
    out.extend_from_slice(&2u16.to_le_bytes()); // cbClientAddress: just null terminator
    out.extend_from_slice(&[0, 0]); // clientAddress: empty
    out.extend_from_slice(&2u16.to_le_bytes()); // cbClientDir
    out.extend_from_slice(&[0, 0]); // clientDir: empty
    out.resize(out.len() + 172, 0); // clientTimeZone (TS_TIME_ZONE_INFORMATION), all zero: UTC
    out.extend_from_slice(&0u32.to_le_bytes()); // clientSessionId (SHOULD be 0)
    out.extend_from_slice(&0u32.to_le_bytes()); // performanceFlags: none set (keep everything on)
    out.extend_from_slice(&0u16.to_le_bytes()); // cbAutoReconnectCookie: none
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved2
    out.extend_from_slice(&0u16.to_le_bytes()); // cbDynamicDSTTimeZoneKeyName
    out.extend_from_slice(&0u16.to_le_bytes()); // dynamicDaylightTimeDisabled: FALSE

    out
}
