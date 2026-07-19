# Build the static marketing site, then serve it from a small nginx image.
FROM node:22-alpine AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
RUN npm run build

FROM nginx:1.27-alpine AS runtime
COPY --from=build /app/dist /usr/share/nginx/html
# nginx:alpine already EXPOSEs 80 and serves /usr/share/nginx/html.
