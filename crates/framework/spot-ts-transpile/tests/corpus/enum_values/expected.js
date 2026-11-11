export let Role = /* @__PURE__ */ function(Role) {
	Role["Owner"] = "owner";
	Role["Member"] = "member";
	return Role;
}({});
export const isOwner = (role) => role === Role.Owner;
