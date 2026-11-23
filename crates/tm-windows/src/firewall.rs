//! 持久 WFP 应用阻断。只操作专用 sublayer 中带版本标记的规则。
//!
//! 使用 ALE connect/receive-accept 的 IPv4/IPv6 四层，不排除回环或限制协议，
//! 因而客户端连本机 HTTP/SOCKS 代理也会被拦截。修改 ALE 策略会触发现有流重授权。
//! 不绑定 provider：没有自动启动服务的 provider 会导致持久规则在 BFE 重启后禁用。
//! 规则持久化由 BFE 负责，不依赖界面/采集助手持续运行。

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{FWP_E_ALREADY_EXISTS, FWP_E_SUBLAYER_NOT_FOUND, HANDLE};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::*;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_WINNT;

use crate::network_policy::{same_path, validate_path, Rule};

const SUBLAYER: GUID = GUID::from_u128(0xa6d92844_b616_41e7_b25b_1077bf128478);
const TAG: &[u8] = b"Tramoing.NetworkBlock.v1\0";
const LAYERS: [GUID; 4] = [
    FWPM_LAYER_ALE_AUTH_CONNECT_V4,
    FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4,
    FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6,
];

fn equal_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn check(code: u32, action: &str) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{action}失败（Windows 0x{code:08X}）；请确认新版采集助手以管理员权限运行，且 Base Filtering Engine 服务可用"))
    }
}

/// 所有句柄、API 分配内存和事务均由 RAII 收回。
pub struct Firewall(HANDLE);

struct WfpMemory<T>(*mut T);
impl<T> Drop for WfpMemory<T> {
    fn drop(&mut self) {
        unsafe { FwpmFreeMemory0((&mut self.0 as *mut *mut T).cast::<*mut c_void>()) };
    }
}

struct Enumeration<'a>(&'a Firewall, HANDLE);
impl Drop for Enumeration<'_> {
    fn drop(&mut self) {
        unsafe { FwpmFilterDestroyEnumHandle0(self.0 .0, self.1) };
    }
}

struct Transaction<'a>(&'a Firewall, bool);
impl Transaction<'_> {
    fn commit(mut self) -> Result<(), String> {
        check(unsafe { FwpmTransactionCommit0(self.0 .0) }, "提交断网规则")?;
        self.1 = true;
        Ok(())
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.1 {
            unsafe { FwpmTransactionAbort0(self.0 .0) };
        }
    }
}

struct Entry {
    id: u64,
    path: String,
    active_layer: Option<usize>,
}

impl Firewall {
    pub fn open() -> Result<Self, String> {
        let mut handle = null_mut();
        let session = FWPM_SESSION0 {
            txnWaitTimeoutInMSec: 3000,
            ..unsafe { std::mem::zeroed() }
        };
        check(
            unsafe { FwpmEngineOpen0(null(), RPC_C_AUTHN_WINNT, null(), &session, &mut handle) },
            "连接系统网络过滤服务",
        )?;
        Ok(Self(handle))
    }

    fn transaction(&self) -> Result<Transaction<'_>, String> {
        check(unsafe { FwpmTransactionBegin0(self.0, 0) }, "开始规则事务")?;
        Ok(Transaction(self, false))
    }

    fn ensure_sublayer(&self) -> Result<(), String> {
        let mut name = wide("Tramoing application network blocks");
        let sublayer = FWPM_SUBLAYER0 {
            subLayerKey: SUBLAYER,
            displayData: FWPM_DISPLAY_DATA0 {
                name: name.as_mut_ptr(),
                description: null_mut(),
            },
            flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
            weight: u16::MAX,
            ..unsafe { std::mem::zeroed() }
        };
        let code = unsafe { FwpmSubLayerAdd0(self.0, &sublayer, null_mut()) };
        if code == FWP_E_ALREADY_EXISTS as u32 {
            Ok(())
        } else {
            check(code, "创建应用过滤层")
        }
    }

    fn entries(&self) -> Result<Vec<Entry>, String> {
        let mut handle = null_mut();
        check(
            unsafe { FwpmFilterCreateEnumHandle0(self.0, null(), &mut handle) },
            "读取断网规则",
        )?;
        let enumeration = Enumeration(self, handle);
        let mut result = Vec::new();
        loop {
            let mut entries = WfpMemory::<*mut FWPM_FILTER0>(null_mut());
            let mut count = 0;
            check(
                unsafe { FwpmFilterEnum0(self.0, enumeration.1, 256, &mut entries.0, &mut count) },
                "枚举断网规则",
            )?;
            if count == 0 {
                break;
            }
            // SAFETY: WFP 返回 count 个过滤器指针，借用不超过本批次 API 内存生命周期。
            for ptr in unsafe { std::slice::from_raw_parts(entries.0, count as usize) } {
                let filter = unsafe { &**ptr };
                if !equal_guid(&filter.subLayerKey, &SUBLAYER)
                    || filter.providerData.data.is_null()
                    || filter.providerData.size as usize > crate::network_policy::MAX_MESSAGE
                {
                    continue;
                }
                let data = unsafe {
                    std::slice::from_raw_parts(
                        filter.providerData.data,
                        filter.providerData.size as usize,
                    )
                };
                let Some(path) = data
                    .strip_prefix(TAG)
                    .and_then(|b| std::str::from_utf8(b).ok())
                else {
                    continue;
                };
                validate_path(path)?;
                let matches_application =
                    if filter.numFilterConditions == 1 && !filter.filterCondition.is_null() {
                        let condition = unsafe { &*filter.filterCondition };
                        equal_guid(&condition.fieldKey, &FWPM_CONDITION_ALE_APP_ID)
                            && condition.matchType == FWP_MATCH_EQUAL
                            && condition.conditionValue.r#type == FWP_BYTE_BLOB_TYPE
                    } else {
                        false
                    };
                let active = filter.flags & FWPM_FILTER_FLAG_PERSISTENT != 0
                    && filter.flags & FWPM_FILTER_FLAG_DISABLED == 0
                    && filter.action.r#type == FWP_ACTION_BLOCK
                    && matches_application;
                result.push(Entry {
                    id: filter.filterId,
                    path: path.into(),
                    active_layer: active
                        .then(|| {
                            LAYERS
                                .iter()
                                .position(|layer| equal_guid(layer, &filter.layerKey))
                        })
                        .flatten(),
                });
            }
        }
        Ok(result)
    }

    pub fn list(&self) -> Result<Vec<Rule>, String> {
        let mut paths = BTreeMap::<String, (String, u8)>::new();
        for entry in self.entries()? {
            let state = paths
                .entry(entry.path.to_lowercase())
                .or_insert((entry.path, 0));
            if let Some(layer) = entry.active_layer {
                state.1 |= 1 << layer;
            }
        }
        Ok(paths
            .into_values()
            .map(|(path, layers)| Rule {
                path,
                complete: layers == 0b1111,
            })
            .collect())
    }

    pub fn set(&self, path: &str, blocked: bool) -> Result<Vec<Rule>, String> {
        validate_path(path)?;
        // 只在新增时检查文件，恢复联网必须支持已被移动/删除的旧 exe。
        let canonical;
        let path = if blocked {
            let resolved = std::fs::canonicalize(path).map_err(|e| format!("无法定位程序：{e}"))?;
            canonical = resolved
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_owned();
            validate_path(&canonical)?;
            if !resolved.is_file() {
                return Err("所选路径不是程序文件".into());
            }
            // 不允许默默将符号链接规则应用到列表里另一个程序。
            if !same_path(path, &canonical) {
                return Err("程序路径包含链接或别名，请选择程序的实际完整路径".into());
            }
            canonical.as_str()
        } else {
            path
        };

        let transaction = self.transaction()?;
        self.ensure_sublayer()?;
        for entry in self
            .entries()?
            .into_iter()
            .filter(|e| same_path(&e.path, path))
        {
            check(
                unsafe { FwpmFilterDeleteById0(self.0, entry.id) },
                "移除旧断网规则",
            )?;
        }
        if blocked {
            let filename = wide(path);
            let mut app_id = WfpMemory::<FWP_BYTE_BLOB>(null_mut());
            check(
                unsafe { FwpmGetAppIdFromFileName0(filename.as_ptr(), &mut app_id.0) },
                "识别程序",
            )?;
            let mut condition = FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_ALE_APP_ID,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_BYTE_BLOB_TYPE,
                    Anonymous: FWP_CONDITION_VALUE0_0 { byteBlob: app_id.0 },
                },
            };
            let mut metadata = [TAG, path.as_bytes()].concat();
            let mut name = wide(&format!("Tramoing: {path}"));
            for layer in LAYERS {
                let filter = FWPM_FILTER0 {
                    displayData: FWPM_DISPLAY_DATA0 {
                        name: name.as_mut_ptr(),
                        description: null_mut(),
                    },
                    flags: FWPM_FILTER_FLAG_PERSISTENT,
                    providerData: FWP_BYTE_BLOB {
                        size: metadata.len() as u32,
                        data: metadata.as_mut_ptr(),
                    },
                    layerKey: layer,
                    subLayerKey: SUBLAYER,
                    weight: FWP_VALUE0 {
                        r#type: FWP_UINT8,
                        Anonymous: FWP_VALUE0_0 { uint8: 15 },
                    },
                    numFilterConditions: 1,
                    filterCondition: &mut condition,
                    action: FWPM_ACTION0 {
                        r#type: FWP_ACTION_BLOCK,
                        Anonymous: unsafe { std::mem::zeroed() },
                    },
                    ..unsafe { std::mem::zeroed() }
                };
                check(
                    unsafe { FwpmFilterAdd0(self.0, &filter, null_mut(), null_mut()) },
                    "应用断网规则",
                )?;
            }
        }
        transaction.commit()?;
        let rules = self.list()?;
        let active = rules.iter().find(|r| same_path(&r.path, path));
        if (blocked && !active.is_some_and(|r| r.complete)) || (!blocked && active.is_some()) {
            return Err("系统规则核验失败，请刷新后重试".into());
        }
        Ok(rules)
    }

    /// 卸载只删除本程序自己的规则；失败必须阻止卸载，保留恢复入口。
    pub fn remove_all(&self) -> Result<(), String> {
        let transaction = self.transaction()?;
        for entry in self.entries()? {
            check(
                unsafe { FwpmFilterDeleteById0(self.0, entry.id) },
                "清理断网规则",
            )?;
        }
        let code = unsafe { FwpmSubLayerDeleteByKey0(self.0, &SUBLAYER) };
        if code != FWP_E_SUBLAYER_NOT_FOUND as u32 {
            check(code, "清理应用过滤层")?;
        }
        transaction.commit()
    }
}

impl Drop for Firewall {
    fn drop(&mut self) {
        unsafe { FwpmEngineClose0(self.0) };
    }
}
