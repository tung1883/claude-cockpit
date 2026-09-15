# Dot-sourced by claude-toast.ps1 — creates (once) a Start Menu shortcut
# carrying our own AppUserModelID + icon. WScript.Shell's COM shortcut
# object can set target/icon/args but NOT the AppUserModelID property —
# that only lives on IShellLinkW's IPropertyStore, which has no simpler
# PowerShell-native path. This is the standard (if verbose) way an
# unpackaged win32/script app gets its own toast identity — the small
# "sender" icon next to the app name in a toast is drawn from whatever
# Start Menu shortcut is registered under that AUMID, not from the toast
# XML itself.
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;

[ComImport, Guid("0000010b-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
public interface IPersistFile {
    void GetClassID(out Guid pClassID);
    [PreserveSig] int IsDirty();
    void Load([MarshalAs(UnmanagedType.LPWStr)] string pszFileName, uint dwMode);
    void Save([MarshalAs(UnmanagedType.LPWStr)] string pszFileName, [MarshalAs(UnmanagedType.Bool)] bool fRemember);
    void SaveCompleted([MarshalAs(UnmanagedType.LPWStr)] string pszFileName);
    void GetCurFile([MarshalAs(UnmanagedType.LPWStr)] out string ppszFileName);
}

[ComImport, Guid("000214F9-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
public interface IShellLinkW {
    void GetPath([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszFile, int cchMaxPath, IntPtr pfd, uint fFlags);
    void GetIDList(out IntPtr ppidl);
    void SetIDList(IntPtr pidl);
    void GetDescription([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszName, int cchMaxName);
    void SetDescription([MarshalAs(UnmanagedType.LPWStr)] string pszName);
    void GetWorkingDirectory([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszDir, int cchMaxPath);
    void SetWorkingDirectory([MarshalAs(UnmanagedType.LPWStr)] string pszDir);
    void GetArguments([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszArgs, int cchMaxPath);
    void SetArguments([MarshalAs(UnmanagedType.LPWStr)] string pszArgs);
    void GetHotkey(out short pwHotkey);
    void SetHotkey(short wHotkey);
    void GetShowCmd(out int piShowCmd);
    void SetShowCmd(int iShowCmd);
    void GetIconLocation([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszIconPath, int cchIconPath, out int piIcon);
    void SetIconLocation([MarshalAs(UnmanagedType.LPWStr)] string pszIconPath, int iIcon);
    void SetRelativePath([MarshalAs(UnmanagedType.LPWStr)] string pszPathRel, uint dwReserved);
    void Resolve(IntPtr hwnd, uint fFlags);
    void SetPath([MarshalAs(UnmanagedType.LPWStr)] string pszFile);
}

[StructLayout(LayoutKind.Sequential, Pack = 4)]
public struct PropertyKey {
    public Guid fmtid;
    public int pid;
    public PropertyKey(Guid fmtid, int pid) { this.fmtid = fmtid; this.pid = pid; }
}

[ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
public interface IPropertyStore {
    void GetCount(out uint propertyCount);
    void GetAt(uint iProp, out PropertyKey pkey);
    void GetValue(ref PropertyKey key, [Out] PropVariant pv);
    void SetValue(ref PropertyKey key, PropVariant pv);
    void Commit();
}

[StructLayout(LayoutKind.Explicit)]
public class PropVariant : IDisposable {
    [FieldOffset(0)] ushort vt;
    [FieldOffset(8)] IntPtr pointerValue;

    public static PropVariant FromString(string value) {
        var pv = new PropVariant();
        pv.vt = 31; // VT_LPWSTR
        pv.pointerValue = Marshal.StringToCoTaskMemUni(value);
        return pv;
    }
    public void Dispose() {
        if (vt == 31 && pointerValue != IntPtr.Zero) { Marshal.FreeCoTaskMem(pointerValue); pointerValue = IntPtr.Zero; }
        GC.SuppressFinalize(this);
    }
    ~PropVariant() { Dispose(); }
}

[ComImport, Guid("00021401-0000-0000-C000-000000000046")]
public class CShellLink { }

public static class AumidShortcut {
    static readonly PropertyKey AppUserModelIdKey = new PropertyKey(new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"), 5);

    public static void Create(string shortcutPath, string targetPath, string iconPath, string aumid, string description) {
        var link = (IShellLinkW)new CShellLink();
        link.SetPath(targetPath);
        if (!string.IsNullOrEmpty(iconPath)) {
            link.SetIconLocation(iconPath, 0);
        }
        link.SetDescription(description);
        var store = (IPropertyStore)link;
        using (var pv = PropVariant.FromString(aumid)) {
            var key = AppUserModelIdKey;
            store.SetValue(ref key, pv);
        }
        store.Commit();
        ((IPersistFile)link).Save(shortcutPath, true);
    }
}
"@

function Ensure-AumidShortcut {
    param([string]$ShortcutPath, [string]$TargetPath, [string]$IconPath, [string]$Aumid, [string]$Description)
    if (Test-Path $ShortcutPath) { return }
    [AumidShortcut]::Create($ShortcutPath, $TargetPath, $IconPath, $Aumid, $Description)
}
