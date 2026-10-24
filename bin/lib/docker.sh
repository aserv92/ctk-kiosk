#!/usr/bin/env bash

. docker/lib/images.sh

docker.build.alpine.image.builder.image() {
  echo "Building Alpine image builder image..."
  docker buildx build \
    --push \
    -t "$ALPINE_IMAGE_BUILDER_IMAGE" \
    --platform linux/amd64 \
    -f docker/alpine-image-builder/Dockerfile docker/alpine-image-builder
}

docker.ensure.alpine.image.builder.image.exists() {
  if [[ -z "$(docker images -q "${ALPINE_IMAGE_BUILDER_IMAGE}")" ]]; then
    docker.build.alpine.image.builder.image
  fi
}

docker.image.pull.no.fail() {
  local image=${1}

  docker pull "$image" || echo "The image $image does not exist."
}

docker.image.pull.all.project.images() {
  docker.image.pull.no.fail "${ALPINE_IMAGE_BUILDER_IMAGE}"
}
