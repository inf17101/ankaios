INCLUDE(FindProtobuf)
FIND_PACKAGE(Protobuf REQUIRED)
INCLUDE_DIRECTORIES(${PROTOBUF_INCLUDE_DIR})
PROTOBUF_GENERATE_CPP(PROTO_SRC PROTO_HEADER /usr/local/lib/ankaios/ank_base.proto /usr/local/lib/ankaios/control_api.proto)
ADD_LIBRARY(proto ${PROTO_HEADER} ${PROTO_SRC})
