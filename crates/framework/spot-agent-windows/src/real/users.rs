//! The NetAPI binding for [`RealWin`](super::RealWin): local users and groups.

use windows::Win32::NetworkManagement::NetManagement::{
    LOCALGROUP_MEMBERS_INFO_3, LOCALGROUP_USERS_INFO_0, NERR_GroupNotFound, NERR_UserNotFound,
    NetLocalGroupAddMembers, NetLocalGroupDelMembers, NetLocalGroupEnum, NetLocalGroupGetMembers,
    NetUserAdd, NetUserEnum, NetUserGetInfo, NetUserGetLocalGroups, NetUserSetInfo,
    UF_ACCOUNTDISABLE, UF_SCRIPT, USER_INFO_0, USER_INFO_1, USER_INFO_1003, USER_PRIV_USER,
};
use windows::core::{PCWSTR, PWSTR};

use super::{RealWin, net, pcwstr, pwstr_to_string, wide};
use crate::api::{LocalGroup, LocalUser, WinApi};
use crate::error::WindowsError;
use crate::secrets::Secret;

impl WinApi for RealWin {
    fn list_local_users(&self) -> Result<Vec<LocalUser>, WindowsError> {
        let names = enumerate_users()?;
        let mut users = Vec::with_capacity(names.len());
        for name in names {
            users.push(self.local_user(&name)?);
        }
        Ok(users)
    }

    fn local_user(&self, name: &str) -> Result<LocalUser, WindowsError> {
        let info = user_info(name)?;
        let groups = user_groups(name)?;
        Ok(LocalUser {
            name: info.0,
            enabled: info.1,
            full_name: String::new(),
            comment: info.2,
            groups,
        })
    }

    fn create_local_user(
        &self,
        name: &str,
        password: &Secret,
        full_name: &str,
        comment: &str,
    ) -> Result<(), WindowsError> {
        let mut name_w = wide(name);
        let mut password_w = wide(password.expose());
        let mut comment_w = wide(comment);
        let mut full_w = wide(full_name);
        let info = USER_INFO_1 {
            usri1_name: PWSTR(name_w.as_mut_ptr()),
            usri1_password: PWSTR(password_w.as_mut_ptr()),
            usri1_password_age: 0,
            usri1_priv: USER_PRIV_USER,
            usri1_home_dir: PWSTR::null(),
            usri1_comment: PWSTR(comment_w.as_mut_ptr()),
            usri1_flags: UF_SCRIPT,
            usri1_script_path: PWSTR::null(),
        };
        // SAFETY: the info struct borrows the wide buffers for the call only.
        let status = unsafe { NetUserAdd(PCWSTR::null(), 1, (&raw const info).cast(), None) };
        let _ = &mut full_w;
        net(status, "NetUserAdd")
    }

    fn set_user_enabled(&self, name: &str, enabled: bool) -> Result<(), WindowsError> {
        let (_, _, flags) = user_flags(name)?;
        let flags = if enabled {
            flags & !UF_ACCOUNTDISABLE
        } else {
            flags | UF_ACCOUNTDISABLE
        };
        let mut name_w = wide(name);
        let info = USER_INFO_1 {
            usri1_name: PWSTR(name_w.as_mut_ptr()),
            usri1_password: PWSTR::null(),
            usri1_password_age: 0,
            usri1_priv: USER_PRIV_USER,
            usri1_home_dir: PWSTR::null(),
            usri1_comment: PWSTR::null(),
            usri1_flags: flags,
            usri1_script_path: PWSTR::null(),
        };
        // SAFETY: the info struct borrows the wide buffer for the call only.
        let status = unsafe {
            NetUserSetInfo(
                PCWSTR::null(),
                pcwstr(&name_w),
                1,
                (&raw const info).cast(),
                None,
            )
        };
        net(status, "NetUserSetInfo")
    }

    fn set_user_password(&self, name: &str, password: &Secret) -> Result<(), WindowsError> {
        let name_w = wide(name);
        let mut password_w = wide(password.expose());
        let info = USER_INFO_1003 {
            usri1003_password: PWSTR(password_w.as_mut_ptr()),
        };
        // SAFETY: the info struct borrows the password buffer for the call only.
        let status = unsafe {
            NetUserSetInfo(
                PCWSTR::null(),
                pcwstr(&name_w),
                1003,
                (&raw const info).cast(),
                None,
            )
        };
        net(status, "NetUserSetInfo")
    }

    fn set_user_groups(&self, name: &str, groups: &[String]) -> Result<(), WindowsError> {
        let current = user_groups(name)?;
        for group in &current {
            if !groups.iter().any(|wanted| wanted == group) {
                self.remove_group_member(group, name)?;
            }
        }
        for group in groups {
            if !current.iter().any(|existing| existing == group) {
                self.add_group_member(group, name)?;
            }
        }
        Ok(())
    }

    fn list_local_groups(&self) -> Result<Vec<LocalGroup>, WindowsError> {
        let mut out = Vec::new();
        for name in enumerate_groups()? {
            out.push(LocalGroup {
                name: name.clone(),
                comment: String::new(),
                members: group_members(&name)?,
            });
        }
        Ok(out)
    }

    fn add_group_member(&self, group: &str, user: &str) -> Result<(), WindowsError> {
        let mut group_w = wide(group);
        let mut member_w = wide(user);
        let info = [LOCALGROUP_MEMBERS_INFO_3 {
            lgrmi3_domainandname: PWSTR(member_w.as_mut_ptr()),
        }];
        // SAFETY: the members array borrows the wide buffers for the call only.
        let status = unsafe {
            NetLocalGroupAddMembers(PCWSTR::null(), pcwstr(&group_w), 3, info.as_ptr().cast(), 1)
        };
        let _ = &mut group_w;
        net(status, "NetLocalGroupAddMembers")
    }

    fn remove_group_member(&self, group: &str, user: &str) -> Result<(), WindowsError> {
        let mut group_w = wide(group);
        let mut member_w = wide(user);
        let info = [LOCALGROUP_MEMBERS_INFO_3 {
            lgrmi3_domainandname: PWSTR(member_w.as_mut_ptr()),
        }];
        // SAFETY: the members array borrows the wide buffers for the call only.
        let status = unsafe {
            NetLocalGroupDelMembers(PCWSTR::null(), pcwstr(&group_w), 3, info.as_ptr().cast(), 1)
        };
        let _ = &mut group_w;
        net(status, "NetLocalGroupDelMembers")
    }
}

/// The account name, enabled flag and comment of a local user.
fn user_info(name: &str) -> Result<(String, bool, String), WindowsError> {
    let name_w = wide(name);
    let mut buffer: *mut u8 = std::ptr::null_mut();
    // SAFETY: NetUserGetInfo fills `buffer` with a USER_INFO_1.
    let status =
        unsafe { NetUserGetInfo(PCWSTR::null(), pcwstr(&name_w), 1, (&raw mut buffer).cast()) };
    if status == NERR_UserNotFound {
        return Err(WindowsError::UnknownUser {
            name: name.to_owned(),
        });
    }
    net(status, "NetUserGetInfo")?;
    // SAFETY: the API returned a USER_INFO_1 at `buffer`.
    let info: USER_INFO_1 = unsafe { std::ptr::read_unaligned(buffer.cast()) };
    Ok((
        pwstr_to_string(info.usri1_name),
        info.usri1_flags & UF_ACCOUNTDISABLE == 0,
        pwstr_to_string(info.usri1_comment),
    ))
}

/// The raw flags of a local user.
fn user_flags(name: &str) -> Result<(String, bool, u32), WindowsError> {
    let name_w = wide(name);
    let mut buffer: *mut u8 = std::ptr::null_mut();
    // SAFETY: NetUserGetInfo fills `buffer` with a USER_INFO_1.
    let status =
        unsafe { NetUserGetInfo(PCWSTR::null(), pcwstr(&name_w), 1, (&raw mut buffer).cast()) };
    if status == NERR_UserNotFound {
        return Err(WindowsError::UnknownUser {
            name: name.to_owned(),
        });
    }
    net(status, "NetUserGetInfo")?;
    // SAFETY: the API returned a USER_INFO_1 at `buffer`.
    let info: USER_INFO_1 = unsafe { std::ptr::read_unaligned(buffer.cast()) };
    Ok((
        pwstr_to_string(info.usri1_name),
        info.usri1_flags & UF_ACCOUNTDISABLE == 0,
        info.usri1_flags,
    ))
}

/// The names of every local user.
fn enumerate_users() -> Result<Vec<String>, WindowsError> {
    let mut buffer: *mut u8 = std::ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    // SAFETY: NetUserEnum fills `buffer` with USER_INFO_0 entries.
    let status = unsafe {
        NetUserEnum(
            PCWSTR::null(),
            0,
            windows::Win32::NetworkManagement::NetManagement::NET_USER_ENUM_FILTER_FLAGS(0),
            &mut buffer,
            u32::MAX,
            &mut read,
            &mut total,
            None,
        )
    };
    net(status, "NetUserEnum")?;
    let mut out = Vec::with_capacity(read as usize);
    for index in 0..read as usize {
        // SAFETY: `buffer` holds `read` USER_INFO_0 entries.
        let info: USER_INFO_0 =
            unsafe { std::ptr::read_unaligned(buffer.cast::<USER_INFO_0>().add(index)) };
        out.push(pwstr_to_string(info.usri0_name));
    }
    Ok(out)
}

/// The names of every local group.
fn enumerate_groups() -> Result<Vec<String>, WindowsError> {
    let mut buffer: *mut u8 = std::ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    // SAFETY: NetLocalGroupEnum fills `buffer` with LOCALGROUP_INFO_0 entries.
    let status = unsafe {
        NetLocalGroupEnum(
            PCWSTR::null(),
            0,
            &mut buffer,
            u32::MAX,
            &mut read,
            &mut total,
            None,
        )
    };
    net(status, "NetLocalGroupEnum")?;
    let mut out = Vec::with_capacity(read as usize);
    for index in 0..read as usize {
        // SAFETY: `buffer` holds `read` LOCALGROUP_INFO_0 entries.
        let info: windows::Win32::NetworkManagement::NetManagement::LOCALGROUP_INFO_0 = unsafe {
            std::ptr::read_unaligned(
                buffer
                    .cast::<windows::Win32::NetworkManagement::NetManagement::LOCALGROUP_INFO_0>()
                    .add(index),
            )
        };
        out.push(pwstr_to_string(info.lgrpi0_name));
    }
    Ok(out)
}

/// The local groups a user belongs to.
fn user_groups(name: &str) -> Result<Vec<String>, WindowsError> {
    let name_w = wide(name);
    let mut buffer: *mut u8 = std::ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    // SAFETY: NetUserGetLocalGroups fills `buffer` with LOCALGROUP_USERS_INFO_0 entries.
    let status = unsafe {
        NetUserGetLocalGroups(
            PCWSTR::null(),
            pcwstr(&name_w),
            0,
            0,
            &mut buffer,
            u32::MAX,
            &mut read,
            &mut total,
        )
    };
    if status == NERR_UserNotFound {
        return Err(WindowsError::UnknownUser {
            name: name.to_owned(),
        });
    }
    net(status, "NetUserGetLocalGroups")?;
    let mut out = Vec::with_capacity(read as usize);
    for index in 0..read as usize {
        // SAFETY: `buffer` holds `read` LOCALGROUP_USERS_INFO_0 entries.
        let info: LOCALGROUP_USERS_INFO_0 = unsafe {
            std::ptr::read_unaligned(buffer.cast::<LOCALGROUP_USERS_INFO_0>().add(index))
        };
        out.push(pwstr_to_string(info.lgrui0_name));
    }
    Ok(out)
}

/// The members of a local group.
fn group_members(group: &str) -> Result<Vec<String>, WindowsError> {
    let group_w = wide(group);
    let mut buffer: *mut u8 = std::ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    // SAFETY: NetLocalGroupGetMembers fills `buffer` with LOCALGROUP_MEMBERS_INFO_3 entries.
    let status = unsafe {
        NetLocalGroupGetMembers(
            PCWSTR::null(),
            pcwstr(&group_w),
            3,
            &mut buffer,
            u32::MAX,
            &mut read,
            &mut total,
            None,
        )
    };
    if status == NERR_GroupNotFound {
        return Err(WindowsError::UnknownGroup {
            name: group.to_owned(),
        });
    }
    net(status, "NetLocalGroupGetMembers")?;
    let mut out = Vec::with_capacity(read as usize);
    for index in 0..read as usize {
        // SAFETY: `buffer` holds `read` LOCALGROUP_MEMBERS_INFO_3 entries.
        let info: LOCALGROUP_MEMBERS_INFO_3 = unsafe {
            std::ptr::read_unaligned(buffer.cast::<LOCALGROUP_MEMBERS_INFO_3>().add(index))
        };
        out.push(pwstr_to_string(info.lgrmi3_domainandname));
    }
    Ok(out)
}
