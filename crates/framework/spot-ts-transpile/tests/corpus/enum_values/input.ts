export enum Role { Owner = "owner", Member = "member" }

export const isOwner = (role: Role): boolean => role === Role.Owner;
