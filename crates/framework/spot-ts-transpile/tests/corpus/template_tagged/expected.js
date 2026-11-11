export const shout = (parts, ...values) => String.raw({ raw: parts.raw }, ...values).toUpperCase();
