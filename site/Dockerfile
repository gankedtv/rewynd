# Build the static marketing site, then serve it from a small nginx image.
FROM node:26-alpine AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
RUN npm run build

FROM nginx:1.31-alpine AS runtime
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /app/dist /usr/share/nginx/html
# nginx:alpine already EXPOSEs 80 and serves /usr/share/nginx/html.

# Probes over `localhost`, which resolves to ::1 before 127.0.0.1 — so this
# fails if nginx ever stops listening on either stack. wget, unlike curl, does
# not retry the other address, which is what makes it the useful probe here.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD wget -q --spider http://localhost/ || exit 1
