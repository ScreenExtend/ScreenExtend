// stubs for nextstepjs

export const useRouter = () => ({
  push: () => {},
  replace: () => {},
  back: () => {},
  forward: () => {},
  refresh: () => {},
  prefetch: () => {},
});

export const usePathname = () =>
  typeof window !== "undefined" ? window.location.pathname : "/";

export const useSearchParams = () => new URLSearchParams();

export const useParams = () => ({});
