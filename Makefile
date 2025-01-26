IMAGE_NAME := sergeydz/concourse-gitlab-merge-request-resource
VERSION := latest

.PHONY: build push all

all: build push

build:
	DOCKER_BUILDKIT=1 docker build -t $(IMAGE_NAME):$(VERSION) .

push:
	docker push $(IMAGE_NAME):$(VERSION)

# Add version tag and push
tag-version:
	@if [ "$(v)" = "" ]; then \
		echo "Please specify version with v=X.X.X"; \
		exit 1; \
	fi
	docker tag $(IMAGE_NAME):$(VERSION) $(IMAGE_NAME):$(v)
	docker push $(IMAGE_NAME):$(v)

# Login to Docker Hub (run this first)
login:
	docker login
